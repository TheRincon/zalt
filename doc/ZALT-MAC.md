# ZALT — A Composable Zstandard Implementation in Rust

**Status:** M0–M2 complete; M3 (Apple Silicon) in progress
**Date:** 2026-08-31 (design draft 2026-08-24)
**Primary target:** aarch64-apple-darwin (M1 and later)
**First consumer:** GRAZER, a WASM-native, JavaScript-free web browser

---

## 1. Context and the layering decision

Zalt began as "a zstd alternative, NEON-optimized." Two constraints have since
reshaped it.

**The wire format is not ours to choose.** The first consumer is a browser, and
origin servers emit `Content-Encoding: zstd` (RFC 8878, IANA-registered). A
browser that cannot decode a standard zstd frame is broken. The wire path is
RFC 8878, bit-exact, non-negotiable.

**But the specialization must not contaminate the codec.** Zalt is meant to be,
in order:

1. A clean, general, stable, and reliable Rust zstd implementation — modern format only, no
   deprecated or legacy cruft.
2. An embeddable library suitable for a browser or app — streaming, bounded,
   hostile-input-safe.
3. A tuned specialization for Apple Silicon, and separately for specific content
   classes such as ZTML (ZDOCs).

Each of those is useful without the ones above it. Someone should be able to
take layer 1 alone as a general-purpose zstd crate and never encounter the word
"browser." That is the organizing constraint of this document.

### The layers

| Layer | Concern | Knows about |
|---|---|---|
| **L1 — Codec** | RFC 8878 format: parse, decode, encode | Bytes. Nothing else. |
| **L1a — Accel** | SIMD kernels behind the same interfaces | CPU architecture |
| **L2 — Embed** | Streaming, memory budget, allocators, pooling, FFI | Host process constraints |
| **L2a — Platform** | OS integration (Darwin VM, dispatch, allocator) | The OS |
| **L3 — Recipes** | Dictionaries + tuning profiles, loaded as *data* | Content classes |

### The dependency rule

**Dependencies point downward only. No lower layer may name a higher one.**

This is a testable invariant, not a convention. In CI:

```sh
! grep -riE 'ztml|zdoc|weft|browser|darwin|apple' crates/zalt-codec/src
! grep -riE 'ztml|zdoc|weft' crates/zalt-embed/src crates/zalt-neon/src
```

If ZTML tuning requires a change inside `zalt-codec`, that is a design failure
to be fixed by widening a *generic* seam, never by adding a content-specific
branch.

### Specialization is data, not code

The mechanism for tuning is a **recipe**: a manifest plus an optional dictionary,
loaded at runtime or baked in at build time. Recipes select dictionaries,
encoder parameters, and decoder policy. They cannot alter the bitstream, so they
can never break correctness — only performance and ratio. ZTML is then not a
feature of zalt; it is *the first recipe*, and it lives in its own crate that
the codec has never heard of. See §7.

---

## 2. Scope

### In scope (P0)

- L1: streaming, incremental **zstd decoder**, RFC 8878 conformant, `no_std + alloc`.
- L2: hostile-input hardening, memory bounding, streaming API, C FFI.
- L1a: NEON kernels for aarch64.
- CLI for benchmarking and differential testing.

### In scope (later)

- zstd **encoder** — correctness first, ratio second (P1).
- L3: recipe mechanism, dictionary loading, `dcz` transport (P1).
- Recipe *trainer* as a dev-time tool (P2).
- `zalt::native` cache codec (P3, gated on measurement — see §10).

### Explicitly out of scope

- **Legacy zstd format versions.** v0.1–v0.7 frame support is cruft; modern
  frames only. This is a deliberate simplification the reference library cannot
  make and we can.
- Beating `zstd -19` on ratio.
- Runtime CPU dispatch (see §5 — unnecessary on aarch64).
- Non-Apple platforms as *tuning* targets. L1 stays portable; only L1a/L2a are
  Apple-specific, and both are optional.

---

## 3. Crate layout

```
zalt/
├── Cargo.toml                  # workspace
├── doc/
│   ├── ZALT-MAC.md             # this file
│   └── showcase.zdoc           # sample ZTML document (L3 reference only)
└── crates/
    ├── zalt-codec/             # L1. no_std + alloc. THE GENERAL ZSTD CRATE.
    │   └── src/
    │       ├── frame.rs            # frame + block headers, skippable frames
    │       ├── literals.rs         # literals section
    │       ├── huf.rs              # Huffman table build + 1/4-stream decode
    │       ├── fse.rs              # FSE table build + decode
    │       ├── sequences.rs        # sequence section, LL/ML/OF
    │       ├── execute.rs          # sequence execution -> output
    │       ├── lz.rs               # match copy, window, wildcopy
    │       ├── bits.rs             # forward + backward bit readers
    │       ├── xxh64.rs            # checksum
    │       ├── dict.rs             # dictionary *loading* (format, not training)
    │       ├── params.rs           # encoder/decoder parameter types
    │       ├── backend.rs          # trait: the seam L1a plugs into
    │       └── encode/             # P1
    ├── zalt-neon/              # L1a. aarch64 kernels implementing `backend`.
    ├── zalt-embed/             # L2. streaming, budget, pool, buffer sources.
    ├── zalt-darwin/            # L2a. mach VM, madvise, dispatch. OPTIONAL.
    ├── zalt-recipe/            # L3 mechanism. Generic. Manifest + dict loading.
    ├── zalt-recipe-train/      # L3 dev tool. Dictionary training, param search.
    ├── zalt-ztml/              # L3 instance. Mostly DATA. Depends on nothing above.
    ├── zalt-ffi/               # C API, hand-written header + ABI drift check
    ├── zalt-cli/               # zalt(1): decompress, info, check
    ├── zalt-fuzz/              # cargo-fuzz targets
    └── zalt-bench/             # criterion harness + corpora
```

### Notes on the seams

**`backend.rs` is the L1/L1a seam.** L1 defines traits for the handful of
operations worth accelerating — wildcopy, overlapping match copy, checksum,
literal bulk copy — and ships a scalar implementation of each. `zalt-neon`
provides an alternative implementation. Selection is compile-time via `cfg`,
not runtime dispatch. The scalar path is never deleted: it is the Miri-testable
correctness oracle (§9) and the portability story.

**`zalt-embed` is content-agnostic.** It knows about memory budgets and
streaming, not about what is being decoded. Browser *policy* (an 8 MiB default
window cap) lives here as a default value; browser *identity* does not.

**`zalt-ztml` is mostly a dictionary and a manifest.** If it accumulates
meaningful amounts of Rust, that is a signal the seam in `zalt-recipe` is too
narrow.

**Why `zalt-neon` and `zalt-darwin` are separate.** aarch64 is not macOS. The
NEON kernels are valid on aarch64 Linux; the Darwin crate is about `mach_vm`,
`madvise`, and `dispatch`. Conflating them would forfeit a portability axis for
no benefit.

---

## 4. L1 — The codec

A clean-room RFC 8878 implementation. Modern frames only.

### Frame

- Magic `0xFD2FB528` (LE).
- Skippable frames: magic `0x184D2A50`–`0x184D2A5F`, 4-byte size, skip payload.
  **Must be handled** — `dcz` dictionary transport uses `0x184D2A5E`.
- `Frame_Header_Descriptor` byte: bits 7–6 `Frame_Content_Size_flag`,
  bit 5 `Single_Segment_flag`, bit 4 unused, bit 3 reserved (must be zero),
  bit 2 `Content_Checksum_flag`, bits 1–0 `Dictionary_ID_flag`.
- `Window_Descriptor` (absent when `Single_Segment_flag`): exponent = high 5 bits,
  mantissa = low 3 bits.
  `windowLog = 10 + exponent`, `windowBase = 1 << windowLog`,
  `windowSize = windowBase + (windowBase / 8) * mantissa`.
- Optional 4-byte content checksum = low 32 bits of XXH64 of decoded content.

### Blocks

- `Block_Header` is 3 bytes LE: bit 0 `Last_Block`, bits 1–2 `Block_Type`,
  bits 3–23 `Block_Size`.
- Types: 0 Raw, 1 RLE, 2 Compressed, 3 Reserved (**reject type 3**).
- `Block_Maximum_Size = min(128 KiB, windowSize)`.

### Literals section

- Types: 0 Raw, 1 RLE, 2 Compressed (new Huffman table), 3 Treeless
  (reuse previous block's table — **stateful across blocks**, a classic source
  of bugs and of use-before-init vulnerabilities).
- Raw/RLE `Size_Format`: `00`/`10` → 1-byte header, 5-bit size;
  `01` → 2 bytes, 12-bit; `11` → 3 bytes, 20-bit.
- Compressed/Treeless `Size_Format`: `00` → 1 stream, 3-byte header, 10+10 bits;
  `01` → 4 streams, 3 bytes, 10+10; `10` → 4 streams, 4 bytes, 14+14;
  `11` → 4 streams, 5 bytes, 18+18.
- 4-stream mode has a 6-byte jump table: three 2-byte sizes for streams 1–3;
  stream 4's size is inferred from the remainder. **Validate against underflow.**
- Huffman `tableLog` max 11; weight-FSE accuracy log max 6.

### Sequences section

- `Number_of_Sequences`: `b0 < 128` → `b0`; `128..=254` → `((b0-0x80)<<8) + b1`;
  `255` → `b1 + (b2<<8) + 0x7F00`.
- `Symbol_Compression_Modes` byte: LL 2 bits, OF 2 bits, ML 2 bits, 2 bits
  reserved (must be zero). Modes: 0 Predefined, 1 RLE, 2 FSE_Compressed,
  3 Repeat (**stateful** — reject Repeat with no prior table).
- Predefined accuracy logs: LL = 6, OF = 5, ML = 6.
- **The sequence bitstream is read backwards.** State init order: LL, OF, ML.
  Per sequence, value bits read offset → match_length → literals_length. State
  updates: LL, ML, OF. Wrong ordering yields plausible garbage; pin with spec
  vectors early.
- Offset codes: `Offset_Value` 1–3 select repeat offsets; else
  `real_offset = Offset_Value - 3`.
- **Repeat-offset update has a special case when `literals_length == 0`.** The
  single most commonly botched part of the format. Test it directly.

### Validation obligations

Input is attacker-controlled. All of these are mandatory:

- Offset ≤ `min(windowSize, bytes_decoded_so_far + dictSize)`.
- Literal lengths sum to the literals section's regenerated size.
- Decoded block size ≤ `Block_Maximum_Size`.
- Total decoded size ≤ `Frame_Content_Size` when declared.
- Reserved bits zero; `Block_Type` 3 rejected.
- FSE/Huffman table construction rejects malformed distributions — accuracy log
  out of range, symbol counts not summing to `1 << accuracyLog`.
- **`Single_Segment_flag` frames: `windowSize` is defined as `Frame_Content_Size`,
  a u64.** Any window cap must cover this branch. Capping only the
  `Window_Descriptor` path leaves the equivalent door open — a likely bug in a
  first implementation.
- **Literals `Regenerated_Size` ≤ `Block_Maximum_Size`.** The field is 20 bits,
  so a three-byte header can claim 1,048,575 bytes — eight times the largest
  legal block. This is the window amplification one layer down, and it must be
  checked *before* the section is decoded, not after: the first version of this
  decoder allocated the full megabyte and only failed later for an unrelated
  reason.
- **`Number_of_Sequences` ≤ `Block_Maximum_Size / 3`.** Every sequence emits a
  match of at least three bytes. The field reaches ~98,000 in three bytes, which
  would otherwise reserve over a megabyte of sequence records before decoding
  one.
- **Zero sequences ends the section.** No symbol-modes byte follows, and any
  trailing bytes make the block corrupt. Accepting them is a validation gap the
  reference's own `zeroSeq_extraneous` fixture catches.
- **The sequence bitstream must be entirely consumed.** Otherwise a truncated or
  over-long bitstream still decodes the declared number of sequences and reports
  success, because the reader returns zero bits past the end rather than failing.
- **Jump table sizes must not exceed the streams payload** (RFC 8878 §3.1.1.3.1.2).
- **A malformed FSE distribution must cost an error, never a hang.**

---

## 5. L1a — Apple Silicon acceleration

### Baseline facts

- **NEON is mandatory on aarch64.** No runtime dispatch, no feature detection,
  no fallback paths. 128-bit vectors; M1 has 4 NEON/FP pipes. A real
  simplification over x86 SSE/AVX2/AVX-512 dispatch — lean on it.
- **SVE is unavailable** on M1–M3. Do not design for it.
- **128-byte cache lines** (vs 64 on x86). Affects prefetch distance, wildcopy
  overrun padding, struct layout.
- **Unaligned loads/stores are cheap.**
- Firestorm is ~8-wide with a deep reorder buffer. It rewards *exposing
  independent dependency chains* far more than clever vectorization of a serial
  loop.

### Ranked wins

1. **4-stream Huffman literal decode.** The format provides four independent
   bitstreams precisely so they can be decoded in parallel. Interleave all four
   in one loop body; the four chains fill the machine width. Likely the largest
   single win, and it requires **no SIMD at all** — just ILP.
2. **Small-offset overlapping match copy.** Offsets < 16 defeat naive vector
   copy. Rather than a byte-loop fallback, build the repeating pattern once and
   expand it branchlessly with `vqtbl1q_u8` over a per-offset shuffle-index
   table. Turns a common worst case into a fast path.
3. **Bulk wildcopy.** `vld1q_u8`/`vst1q_u8` for 16/32-byte unaligned copies,
   overrunning into padded output slack (≥32 B, ideally to a 128 B line). This
   is also the dominant path for incompressible input, which decodes as raw
   literals and raw blocks at near-`memcpy` speed.
4. **FSE state interleaving.** Serial per state, but LL/ML/OF are independent.
   Interleave them; consider decoding a sequence ahead where the bit budget allows.
5. ~~**Double-symbol Huffman tables** for low-entropy literals — two symbols per
   lookup.~~ **Struck at M3.** Measured 8.4% slower on real `.zdoc`: the paired
   table costs `1 << table_log` to build per frame, and pairing conflicts with
   win #1 above, because streams emitting one or two symbols per step can no
   longer be interleaved on a shared index. See §10.
6. ~~**XXH64 with NEON.** Four 64-bit accumulators map to two Q registers.~~
   **Struck at M3.** AArch64 NEON has no 64-bit integer multiply, so this
   cannot be done without emulating every product from 32-bit pieces — slower
   than the scalar multiply Firestorm issues two of per cycle. The real win in
   the checksum was not computing it at all when the frame declares none; see
   §10.
7. ~~**Match-source prefetch** one sequence ahead.~~ **Struck at M3.**
   Implemented behind the backend seam and measured at a consistent **3.5%
   regression** in an isolated A/B. The premise is that a match source misses
   cache; it does not. A 2 MiB window — and even the 8 MiB browser ceiling —
   sits inside the M1 P-cluster's 12 MiB shared L2, so the load is already
   ~15 cycles and an out-of-order core hides it without help. What the hint
   added was instructions, and an address computation needing three integer
   divisions to find the *future* cursor position in a ring. Worth revisiting
   only for windows that genuinely exceed L2.

### Not worth doing

- Vectorizing the bit reader itself — the dependency chain through the bit
  accumulator defeats it. Use cross-stream ILP instead.
- Table-driven vector gather. NEON has none; `vqtbl` indexes only within
  registers, 16 bytes per table register.

### Measured at M0 — and the lesson

First kernel, M1, match copy, `cargo run --release -p zalt-bench -- kernels`:

| shape | scalar GB/s | neon GB/s |
|---|---:|---:|
| offset=1, len=64 (byte smear) | 0.62 | 3.12 |
| offset=7, len=256 | 1.27 | 12.38 |
| offset=15, len=256 (widest overlapping) | 1.88 | 12.54 |
| offset=16, len=256 (boundary) | 6.33 | 9.74 |
| offset=64, len=256 | 16.35 | 34.97 |
| offset=1024, len=4096 (bulk) | 34.14 | 59.67 |

Single-run figures on an M1 with meaningful run-to-run variance; treat them as
shape, not as precision.

**The first version of the bulk row was a 40% regression** — 14.7 GB/s NEON
against 24.6 scalar. A flat one-vector-per-iteration intrinsic loop lost to the
scalar code, because LLVM auto-vectorises *and unrolls* the scalar loop into
wider accesses than the hand-written version used. The fix was to tier the loop
width against `offset` and delegate the truly non-overlapping case to
`copy_within` (platform `memmove`).

The lesson is worth keeping, because it predicts which items on the ranked list
will pay: **hand-written NEON earns its place where the compiler cannot help.**
Short overlapping offsets carry a loop-carried dependency that blocks
auto-vectorisation entirely — a 6–10x win. Bulk non-overlapping copy is
something the compiler and libc already do well, and hand-writing it is a
regression waiting to be measured. This is the same reasoning that led
`copy_literals` to delegate to `memcpy` rather than vectorise by hand.

Ranked-list items #1 (4-stream Huffman ILP) and #2 (small-offset overlap) are
both "compiler cannot help" cases. Item #3 (bulk wildcopy) is not, and should be
expected to yield little.

**Measured at M3, and the ranking did not survive.** Item #1 was worth ~5% on a
real corpus; the two changes that mattered — a word-at-a-time bit reader and a
wrapping match copy that reaches the kernels at all — appear nowhere on this
list. The list ranks *kernels*, and the decoder's time was going to the code
around them. §10's M3 notes carry the numbers. Nothing above is wrong about the
architecture; it was simply never checked against a profile, and this section
said so about itself two paragraphs ago.

### Measurement discipline

Nothing lands without a benchmark. Build `zalt-bench` *before* the optimization
phase. Compare against system `zstd` 1.5.7 (`/opt/homebrew/bin/zstd`).

Use `cargo-instruments` / Instruments for cycle-level work; the M1 PMU gives
cache-miss and branch-miss attribution worth having.

**Corpus selection is a recipe concern, not a codec concern** — see §7. The
benchmark harness takes a corpus directory as input and has no built-in opinion
about content.

---

## 6. L2 — Embedding

Everything a host process needs and a codec should not contain.

### Streaming is mandatory

Consume socket-sized chunks, produce output incrementally, suspend and resume at
*arbitrary byte boundaries* — mid-header, mid-block, mid-literals. A pull-based
`decompress(&mut self, input, output) -> Status` interface is the model. Never
require a whole response body in memory. This shapes the internal design, so
build it in from the first commit rather than retrofitting.

### The window clamp

The window is the LZ77 back-reference limit, and therefore the decoded history
the decoder must keep resident. It is not a buffer that can be flushed: a
sequence arriving later may reference any byte in it. It is irreducible resident
memory for the frame's lifetime — and it is declared by the sender.

`Window_Descriptor` allows `windowLog` up to 41, so a **spec-legal ~6-byte frame
header can demand a 2 TiB window** (libzstd's own refusal above `windowLog` 31 is
implementation policy, not format). That is an unauthenticated remote
memory-exhaustion primitive with unbounded amplification, reachable from any
origin on any subresource.

Two independent defenses; both are needed:

- **Lazy allocation** — never hold more than
  `min(declaredWindow, bytes_actually_decoded)`. A frame producing 10 KB needed
  10 KB of history regardless of its header. This alone defeats header
  amplification, and libzstd's streaming path does *not* do it, so it is a real
  differentiator. It is also a **performance** requirement wherever many small
  resources decode concurrently: touching a declared 8 MiB window per resource
  would be a storm of page faults for data never used.
- **A hard cap, checked at header parse** — covers what lazy allocation cannot:
  an attacker sending *real* content. zstd compresses degenerate input at
  enormous ratios, so a modest request can legitimately decode to gigabytes, and
  matches placed at maximum distance force retention of the whole window.
  Reject-early matters: you cannot decode with a smaller window than declared and
  hope, because you would fail partway through when a long-distance match arrives.

**On the value.** Security says "have a cap"; it does not pick a number. 8 MiB is
an *ecosystem* observation, not a Chrome-compatibility requirement: RFC 8878 names
8 MB as the interop floor decoders should support, and public browser caps have
made it the de facto ceiling that origins already target. Below it you reject
legitimate content; above it you accept content nobody sends while raising your
worst case. It is a **default in a config struct**, not a constant, and it applies
only to untrusted input — a trusted local cache needs no cap.

### Per-frame caps are not sufficient

8 MiB is *per decoder instance*. A hostile page referencing a hundred
zstd-encoded subresources, decoded concurrently, authorizes 800 MB.

L2 therefore owns a **global budget across live decoders** — a bounded decoder
pool or an accounting allocator that applies backpressure to the network layer.
This makes caller-provided allocators and footprint reporting load-bearing
requirements rather than conveniences. Decoder instances must be cheap to
`reset()` and reuse; prefer pooling over per-response allocation.

### Hostile input

The strongest argument for Rust here. Historically, browser decompressor CVEs are
memory-safety bugs in C.

- `#![forbid(unsafe_code)]` on all parsing and table-construction code.
- `unsafe` confined to audited SIMD/copy kernels in `zalt-neon` and `lz.rs`,
  each with a documented safety contract and bounds established by the caller.
- No `unwrap`/`panic` reachable from parsing; enforced by lint and fuzz target.
- Panics must never unwind into Swift/ObjC: `catch_unwind` at the FFI boundary,
  error codes not panics.

---

## 7. L3 — Recipes: the tuning mechanism

The composability answer. A **recipe** is a manifest plus an optional dictionary,
consumed as data. Recipes tune; they never alter the bitstream.

### What a recipe can and cannot control

| Can | Cannot |
|---|---|
| Which dictionary to use | The frame/block format |
| Encoder parameters (level, windowLog, strategy, literal mode) | Decoded output bytes |
| Decoder policy (window cap, budget, size limits) | Validation rules |
| Content-type → profile routing | Anything in L1 |
| Whether to attempt compression at all (stored-block fast path) | Wire compatibility |

### Filters — rejected for now

Pre/post-processing filters are **out of scope**. Not measured, not prototyped,
deliberately deferred. Recorded here only so the reasoning is not re-derived:

- A filtered stream is still a well-formed zstd frame, but it decodes to
  `F(document)`. **If it is ever revisited, it must be served under its own
  content-coding token** (`Accept-Encoding: zstd-ztml`), never under `zstd`.
  Caching intermediaries key on `Content-Encoding`; filtered bytes served as
  `zstd` will eventually be cached and handed to a generic client, corrupting
  content in a way neither endpoint can detect. This is the mechanism `dcz`/`dcb`
  used, and the reason they used it.
- The cost is two byte-exactly reversible implementations (Go and Rust) in
  lockstep, cross-language differential fuzzing, and a new attacker-controlled
  parser on the untrusted path. A dictionary is inert data; a filter is code.

Architecturally a filter would sit above L1 (`bytes → filter → zstd → wire`), so
nothing in the current design forecloses it later.

### Block splitting (encoder, wire-compatible)

Unrelated to filters and worth doing on its own: zstd permits arbitrary block
boundaries, and **each block builds its own literals Huffman table**. An encoder
that splits blocks at statistical transitions — compressible markup in one block,
incompressible hash/signature regions in another — lets each get a fitted table
instead of forcing one table to cover both. Today they interleave and both
compress worse for it.

This needs no token, no negotiation, and no server-side coordination: it is
entirely an encoder-side decision within RFC 8878, and any decoder handles the
output. Tracked in M5.

### Sketch

```toml
[recipe]
name = "ztml-0.3"

[match]
content_type = ["application/ztml"]
extension    = [".zdoc"]

[dictionary]
path = "ztml-0.3.dict"
id   = 0x5A544D4C          # carried in Dictionary_ID; both peers resolve it

[encode]
level = 6
window_log = 17

[decode]
max_window = "8MiB"

[[rule]]                    # content already compressed: don't bother
match_content_type = ["image/webp", "image/png", "image/avif", "font/woff2"]
strategy = "stored"
```

Recipes load from a directory, from an embedded blob compiled into the binary, or
ad hoc from a path — the browser bakes ZTML in; the CLI takes `--recipe foo.toml`.

### Dictionaries

Two very different jobs, and the split is favorable:

- **Loading** (decode side) is easy and belongs in L1 `dict.rs`: parse the
  dictionary format, install the entropy tables and content prefix.
- **Training** (encode side) is the real work — COVER/fastCover-style selection —
  and lives in `zalt-recipe-train` as a **dev-time tool**, entirely off the
  browser's critical path.

Staging: v1 uses a **content-prefix dictionary**, which is just a concatenation
of representative samples and needs no algorithm at all. It works surprisingly
well and unblocks the whole mechanism. Upgrade to trained dictionaries later.

`zalt recipe train --input corpus/ --out ztml.recipe` is the ad-hoc path: point
it at a directory of representative files, get a dictionary and a parameter set.

### Why dictionaries matter disproportionately here

A zstd dictionary carries **pre-computed Huffman and FSE tables**, not only a
content prefix. Entropy table construction is a *per-frame fixed cost*, so on
small resources it can rival actual decoding. Any workload dominated by many
small files gets a double win — better ratio and less setup.

**Caveat, and it is a real one:** a dictionary-compressed frame can only be
decoded with the dictionary that compressed it. A client-side dictionary is
therefore worthless on the wire unless origins adopt the same one. The mechanism
is a well-known `Dictionary_ID` both peers resolve out of band — which removes
*per-request* negotiation but still requires ecosystem adoption. Where we own
both ends (a local cache), the win is unilateral and free. Do not plan as though
shipping a dictionary in a browser binary unilaterally improves wire performance;
it does not.

`dcz` (Compression Dictionary Transport) is the standardized alternative for
negotiated dictionaries: a 40-byte prelude — skippable magic `0x184D2A5E`,
length `0x20`, then the dictionary's 32-byte SHA-256 — ahead of an ordinary
frame. Since skippable frames and dictionary IDs are required anyway, keeping
this in the decoder's shape from the start avoids a painful refactor.

---

## 8. L3 instance — the ZTML profile

**This section is the only place in the document where ZTML appears, and
`zalt-ztml` is the only crate that may reference it.** It is recorded here to
justify the recipe's contents, not to influence L1.

### What a `.zdoc` looks like to a compressor

Three statistically distinct regions coexist (see `doc/showcase.zdoc`):

1. **Structural markup** — heavy short-range repetition (`<section>`,
   `<th scope="row">`, `</tr>`) plus space-run indentation on nearly every line.
   Produces **small-offset overlapping matches** — exercises §5 win #2.
2. **The WEFT data layer** — near-template repetition at medium distance. Every
   `claim` block repeats `subject:`/`predicate:`/`object:`/`@warrant:`, and URIs
   and prefixes (`<urn:concept:ztml>`, `schema:`, `hist:`) recur. Excellent match
   material at offsets in the hundreds of bytes, and ideal dictionary material.
3. **Mandatory incompressible tokens** — the root element carries an ed25519
   signature (88 chars base64) and public key (44 chars); entities are
   `urn:uuid:` identifiers; and because `hash` is **mandatory rather than
   advisory**, every subresource reference carries a 64-character SHA-256 hex
   string.

Region 3 is the distinctive one: a reference-dense `.zdoc` has a large
incompressible fraction no match finder can touch, all of it flowing through the
Huffman literals path. Hex draws from 16 symbols and base64 from 64, so Huffman
earns real gains — but these regions are **literal-dominated and match-poor**.

**Consequence: the literals path is disproportionately hot.** This corroborates
ranking 4-stream Huffman decode first in §5 — but note the ranking was derived
from architecture, not from ZTML, and stands without it.

### The push model

Subresources are hash-pinned and pushed on their own streams in the same round
trip: **many small resources decoded concurrently**. Per-frame fixed cost
dominates over steady-state throughput. This is the workload that most rewards
dictionaries (§7) and lazy window allocation (§6), and it stresses the global
memory budget more than any single large transfer would.

ZTML's **closed vocabulary** makes it an unusually good dictionary target — the
entire tag set and WEFT keyword set is known at build time. Subject to the
adoption caveat in §7.

### Corpus for the ZTML recipe

| Category | Notes |
|---|---|
| `.zdoc`, small (1–16 KB) | Dominant case. Per-frame overhead rules. |
| `.zdoc`, large | Long articles; steady-state throughput. |
| `.zdoc`, WEFT-dense | Many `entity`/`claim` blocks — template matching. |
| `.zdoc`, reference-dense | Many hashes/UUIDs — worst-case literal entropy. |
| WASM modules (ZAPPs) | Binary; LEB128 varints, opcode streams. Distinct stats. |
| WebP / PNG / AVIF / AV1 | Already compressed — throughput path, see below. |
| Fonts (WOFF2) | Already compressed (Brotli inside). |

Explicitly **not** in this corpus: HTML, CSS, JavaScript, Silesia, enwik. None
reflect what this browser fetches.

**On pre-compressed media.** WebP/PNG/AVIF/AV1 payloads are already compressed;
zstd over them yields ~1.0x while costing full decode time, and origins should
serve them `identity`. They belong in the corpus as a **throughput** measurement,
not a ratio one — incompressible input travels the raw-literal and raw-block
paths where the target is near-`memcpy`. The recipe's `strategy = "stored"` rule
(§7) is what keeps the encoder from wasting cycles on them.

---

## 9. Correctness and testing

A security boundary. Testing is not a phase at the end.

### The reference corpus

`zstd-fixtures/` holds facebook/zstd's own regression files, and they are worth
more per byte than anything generated, because each one is a trap somebody
already fell into:

| Set | What it pins |
|---|---|
| `golden-decompression/` | Frames that must decode exactly — including `rle-first-block` (a block type encoders almost never emit) and `zeroSeq_2B` (the zero-sequences edge case) |
| `golden-decompression-errors/` | Frames that must be **refused** |
| `golden-compression/` | Inputs that stress specific encoder paths, and therefore the matching decoder paths — `large-literal-and-match-lengths` reaches the high LL/ML codes with their wide extra-bit fields |
| `golden-dictionaries/` | For M4 |

Derived artifacts (expected outputs, compressed forms) are generated by
`just zstd-fixtures` rather than committed.

**That policy broke CI the first time the repository was pushed**, and the shape
of the failure is worth keeping. The golden corpora are committed; the artifacts
derived from them are not; and nothing generated them before the tests ran. The
conformance tests *skip* when `zstd-fixtures/` is missing entirely and **fail**
when it is present without its derived files — which is precisely a fresh clone,
so the graceful path guarded the case that never happens and not the one that
does. `golden_error_frames_are_rejected` passed throughout, because it needs no
derived files; the two that compare against expected output did not.

Fixed by making `fast-test`, `test-release` and `miri` depend on a private
`_fixtures` recipe that generates them when absent and costs one `ls` when they
are not. Deliberately *not* fixed by adding a step to the workflow: CI runs the
same recipes as the laptop so the two cannot drift, and a CI-only step is drift
by construction — the next person to run the tests a different way would have
hit the same wall.

A second, quieter version of the same bug was sitting next to it. `zstd` was
pinned to `/opt/homebrew/bin/zstd`, an absolute path that **cannot exist on the
Linux runners**, so every fixture-deriving recipe was macOS-only and the
`portable` and `miri` jobs would have failed even with the step added. It is now
found on `PATH`, with `ZSTD=` still overriding.

**The errors directory is the important one.** Round-trip tests only ever prove
a decoder is permissive enough; nothing in them detects a decoder that is *too*
permissive. It found a real gap on first run — see below.

### Fuzzing

Five targets in `fuzz/`, seeded from both fixture sets by `just fuzz-seed`.

| Target | Property |
|---|---|
| `decode_hostile` | Arbitrary bytes never panic, hang, or allocate without bound |
| `decode_differential` | Against libzstd — see the asymmetry note below |
| `roundtrip_valid` | libzstd compresses, zalt must reproduce exactly |
| `streaming_equivalence` | Chunked decode equals one-shot, with output discarded between calls |
| `backend_equivalence` | Scalar and NEON agree byte for byte |

**Random bytes are nearly useless on their own.** They essentially never form a
valid zstd frame past the magic number, so an unseeded fuzzer hammers header
validation and never builds an entropy table. `roundtrip_valid` exists to reach
the decode paths: the fuzzer's bytes are the *content*, libzstd compresses them,
and every mutation changes the entropy tables and streams that come back.

**The differential oracle must be asymmetric, and weaker than it looks.** zalt
deliberately supports less than libzstd (no legacy frames, no dictionaries yet,
a window cap). And a frame *without a content checksum* carries no way to detect
a flipped bit, so two conformant decoders may produce different garbage and
neither is wrong — a mutating fuzzer generates that case constantly. What
survives:

- zalt accepts what libzstd rejects → **always a bug**, a hole in validation.
- Both accept and the frame declares a checksum → outputs must be identical
  (both verified the same XXH64, so this is meaningful).
- Both accept, no checksum → no assertion is possible.
- zalt rejects what libzstd accepts → fine; being stricter is allowed.

The unconditional output check lives in `roundtrip_valid`, where frames are
valid by construction and none of that reasoning is needed.

### Other layers

- **Memory safety**: fuzz corpus under ASan; Miri over the scalar path (Miri
  cannot execute NEON intrinsics — the retained scalar backend is what makes
  this possible at all, and doubles as the correctness oracle).
- **Layering CI**: the `grep` invariants from §1. A violation fails the build.
- **No `unwrap`/`panic` reachable from parsing.**

## 10. Milestones

### M0 — Skeleton ✅
- [x] Workspace, crate layout, CI (build/test/clippy/fmt + layering greps).
- [x] `backend.rs` trait seam with the scalar implementation.
- [x] `zalt-bench` harness taking a corpus directory, comparing vs system `zstd`.
- [x] `zalt-neon` first kernel + exhaustive `backend_equivalence` tests.
- [x] Magic-number registry with collision tests.

- [x] `justfile` — one entry point for every task; CI runs the same recipes, so
      the local loop and CI cannot drift apart.

`ci/layering.sh` enforces the §1 contract. Corpus is unpopulated — the baseline
mode has nothing to measure until real `.zdoc` traffic is captured.

`just gate` runs everything CI runs, in CI order. `just t` is the inner loop
(nextest, parallel, sub-second). `just fixtures` regenerates the interop
fixtures **deterministically** — from AES-CTR over zeros rather than
`/dev/urandom`, because fixtures built from real randomness change on every
regeneration, silently changing what the suite tests and making a failure
impossible to reproduce.

### M1 — Correct scalar decoder (critical path) — decoding works end to end
- [x] Frame/block headers, skippable frames.
- [x] Raw and RLE blocks.
- [x] XXH64 checksum, verified against real `zstd` frame output.
- [x] Streaming state machine — suspend/resume at arbitrary boundaries.
- [x] Window clamp incl. the `Single_Segment` content-size path.
- [x] Decoded size bounded *during* decode, not after.
- [x] Forward (LSB-first) and backward (MSB-first) bit readers.
- [x] FSE table description parsing, table construction, predefined tables.
- [x] Huffman tree description (direct + FSE-compressed weights), table build,
      1-stream and 4-stream decode.
- [x] Literals section: raw / RLE / Huffman / treeless.
- [x] Sequence decode — backward bitstream, all three orderings.
- [x] Sequence execution and repeat offsets **including `literals_length == 0`**.
- [x] Window management as a bounded ring buffer, lazily grown.
- [x] Reference conformance corpus (`zstd-fixtures/`) passes — golden frames
      decode exactly, golden **error** frames are refused, golden compression
      inputs round-trip at levels 1/3/9/19.
- [x] Differential, round-trip, streaming and backend fuzz targets.
- [x] Validation set from §4 complete (see below).
- [x] Sustained fuzz campaign — see below. `just fuzz-all` cycles targets
      briefly and is a smoke test; a real run pins one target with `-workers=N`
      and detaches.

**A real `zstd -3` compressed frame now decodes byte for byte** — four-stream
Huffman literals with FSE-coded weights, an FSE-coded sequences section, and
match execution through the backend seam. Both backends agree on it.

#### Two spec bugs, and why the tests missed them

**The FSE weight-stream termination.** Emitting a symbol *includes* advancing
its state, and the end-of-stream test belongs **after** that advance. Testing
before it — the intuitive reading — stops one symbol early, shifting every
symbol index past the truncation point down by one.

**The `ML_DEFAULT` distribution.** The predefined match-length distribution's
`-1` run begins at index 46, not 48: 37 ones and 7 minus-ones, not 39 and 5.

The second is the more instructive failure. The wrong split **sums to 64 just
like the right one**, so `predefined_distributions_sum_to_their_table_size`
passed, the table built, and every internal-consistency check held. The only
symptom was that some states decoded to the wrong symbol — and the entire
existing fixture set used `FSE_Compressed` mode, so the *predefined* tables were
never exercised by a single test. A whole code path was unverified and nothing
said so.

Both bugs are now pinned by tests that check *shape*, not totals
(`predefined_distributions_have_the_exact_shape_from_the_rfc`) and by golden
state-to-symbol assertions taken from a real frame.

RFC 8878 is vendored at `doc/rfc8878.txt`. Both bugs were invisible to
inference and obvious on reading the spec text; that is worth the 100 KB.

#### What the first real campaign found

All five targets, multi-worker, under ASan, seeded from both fixture sets:

| target | runs | result |
|---|---:|---|
| `decode_differential` | — | **two bugs** (below) |
| `decode_differential`, after both fixes | 1,016,826 | clean |
| `decode_hostile` | 216,565 | clean |
| `backend_equivalence` | 26,916 | clean — no scalar/NEON disagreement |
| `roundtrip_valid` | 40,055 | clean |
| `streaming_equivalence` | 7,379 | clean |

The corpus grew roughly fourfold across the run and is worth keeping; §10's
note about `just clean-corpus` applies.

**Miri needed gating, not fixing.** The exhaustive oracles added in M3 — every
bit position and width, every combination of ring capacity, cursor phase, offset
and length — are seconds natively and unbounded under an interpreter; a first
run burned 34 minutes of CPU without finishing. Each now scales itself down
under `cfg(miri)` the way `conformance.rs` already did, keeping the property
while dropping the cross-product. The scalar path is clean in 221 seconds.

`backend_equivalence` mattered most for confidence in the M3 work — it is what
holds the new `unsafe` in `zalt-neon`'s small-copy kernels against the scalar
oracle — and `roundtrip_valid` is what proves the two new validation checks do
not refuse frames libzstd actually produces.

Every crash was a 19-byte frame, and every one was the asymmetry §9 names as
the only unconditionally meaningful one: **zalt accepted what libzstd
rejects.**

The frame declares seven literals under a Huffman table whose codes are one or
two bits, so at most fourteen bits are consumed — against twenty readable bits
in the stream. zalt produced the correct seven bytes and reported success.

**The obligation was already written down, one section up, for the wrong
section.** §4 requires that *the sequence bitstream* be entirely consumed, and
explains exactly why: "a truncated or over-long bitstream still decodes the
declared number of sequences and reports success, because the reader returns
zero bits past the end rather than failing." Every word of that applies to a
Huffman literals stream, `sequences.rs` implemented it, and `huf.rs` never did.
The reference refuses these frames at `BIT_endOfDStream` for the same reason.

`check` was not enough on its own: it catches reading *past* the end, and this
is the opposite failure. Both are now required, in the one-stream and
four-stream paths alike, and the three frames are pinned as
`literal_streams_with_unconsumed_bits_are_refused` rather than left in
`fuzz/artifacts/`, which `just clean-fuzz` deletes.

**The second bug, found by re-running the campaign after fixing the first.**
A Huffman tree description whose declared depth no symbol reaches. Weights
`[0, 2, 0]` derive a final weight of 2, giving `table_log` 2 — a four-entry
table — for two symbols whose codes are one bit each. Since `nb_bits` is
`table_log + 1 - weight`, a symbol of weight 1 is the one that actually uses
the full depth, and this description has none. The same code written minimally
is `[1, 1]` at `table_log` 1, which both decoders accept.

It decodes perfectly well, which is exactly why nothing objected: the spans sum
to the table size, every state is populated, the stream is fully consumed, and
the output is the seven bytes the bits imply. Nothing downstream can tell that
every lookup read a bit it never consumed.

Finding it took bisection rather than inspection, and the sequence is worth
recording because the first three hypotheses were all wrong:

1. Rebuilt the same table with a **direct** weight description instead of an
   FSE-compressed one. Both decoders behaved identically → the FSE weight path
   was not at fault.
2. Same framing with **raw** literals → both accept, so the block and frame
   layout were fine.
3. A **flat four-symbol** table → both reject, so ordinary table construction
   was fine.
4. The same code written **minimally** → both accept.

Only (4) separated them, and it names the rule. Guessing at libzstd's internals
produced nothing; four controls that differed in one variable each produced a
one-line check.

Two things worth keeping from this:

- **The generalisation was available and nobody made it.** A validation
  obligation discovered for one entropy-coded stream is a hypothesis about
  every entropy-coded stream. That is a cheap review question and it would have
  found the first bug without a fuzzer.
- **Fixing one bug is what exposed the next.** The second frame was reachable
  only once the first no longer stopped the campaign, and the same is likely
  true again — a clean run means "no more findings at this depth", not "no more
  bugs". Both fixes are the same shape: a structure that decodes without
  complaint but that no encoder would ever emit.
- **It was not a regression from the M3 optimisation work**, which was the first
  suspicion — `entry_masked` deliberately traded an error for a masked index.
  An assertion added temporarily in its place never fired on these inputs,
  which ruled it out before any time went into it. Fuzzing a decoder right
  after rewriting its hot paths is worth doing for exactly this: the campaign
  distinguishes "the change broke it" from "it was always broken".

#### The bug worth remembering

The first end-to-end attempt produced 15366 of 15366 bytes with a handful wrong:
`entitx` where `entity` belonged. `x` never appears in the source at all, so the
symbol index was off by one — and only for the *highest* symbol value, because
the Huffman tree description omits the last symbol's weight and derives it.

The cause was the FSE weight-stream termination. Emitting a symbol *includes*
advancing its state, and the end-of-stream test belongs **after** that advance.
Testing before it — the intuitive reading — stops one symbol early, shifting
every symbol past the truncation point down by one.

What makes it worth recording is how nearly invisible it was: the table still
built, the checksum was the only thing that objected, and had the input happened
to use every byte value the error would have been spread across the output
instead of concentrated in one rarely-used symbol. This is precisely the failure
class the differential fuzzing in §9 exists to catch, and an argument for
prioritising it.

#### The window

History now lives in the decoder, not in the caller's buffer. Before, matches
indexed into the output `Vec`, so a caller had to retain an entire frame — a
500 MB response could not be streamed to disk. `caller_may_discard_output_between_calls`
is the test that pins the new property: the caller clears its buffer after every
call and decoding stays correct.

A **ring** rather than a sliding buffer with compaction. Compaction keeps history
contiguous — so the accelerated kernels always apply — but costs write
amplification of `window_size / slack`; at 8 MiB with 128 KiB slack that is 64x,
and making it cheap means roughly doubling memory. The ring costs neither, at the
price of a match spanning the wrap point not being one flat operation. That
trade is right here because **a frame smaller than its window never wraps**, so
the common case stays on the flat path that reaches the NEON kernels, and only
frames exceeding their own window pay.

Capacity is a ceiling, not a reservation: a frame declaring 8 MiB and producing
4 KB allocates 4 KB.

Miri is clean over the scalar path.

### M2 — Embedding ✅
- [x] Global budget with **resizable leases**, footprint reporting.
- [x] `zalt-embed::Decoder` — policy-bound, budget-charged, streaming.
- [x] `DecoderPool` — reset-and-reuse, bounded idle set, shrink-on-return.
- [x] Caller-provided allocator — `BufferSource` + `Recycler`.
- [x] `zalt-ffi` C API + header, panic-proof boundary.
- [x] `zalt-cli`.

The header is **hand-written rather than cbindgen-generated**, which is a
deliberate reversal of the plan above. A generated header follows whatever the
Rust happens to look like, so an accidental signature change silently becomes
the new API; a hand-written one *is* the contract. The cost of hand-writing is
drift, and `ci/check-abi.sh` is what pays it — it compares exported symbols and
constant *values* between the two, because a constant that disagrees is worse
than one that is missing: the caller links fine and misreads every error.
`just ffi-smoke` then compiles a real C program against the staticlib, which is
the part that otherwise breaks at integration time rather than in CI.

#### Accounting against a footprint that grows

The awkward part, and the reason leases resize.

A decoder's memory is not known when it starts, because the window is allocated
lazily. Reserving the declared maximum up front is simple and wrong: it charges
every response for memory it never touches, and would refuse the ninth
concurrent 4 KB response on a 64 MiB budget. Reserving nothing is not accounting
at all.

So a `Lease` tracks *actual* usage, re-checked after every `decompress` call.
When the budget refuses growth the decode returns `Error::OverBudget` — its own
variant, not a parse error, because the caller can act on it: queue the
response, retry when other decoders finish, or fail one resource instead of the
page.

#### Why pooled decoders are shrunk before going idle

A decoder that once served a large response keeps a large buffer and keeps it
*reserved* while idle. Left alone, a pool converges on every slot holding the
high-water mark of anything it ever decoded — budget charged permanently for a
peak that has passed. A decoder returning above `idle_footprint_max` is shrunk
instead, trading one allocation on its next use for not holding the peak.

#### The allocator is a buffer source, not an allocator

"Caller-provided allocator" reads as a per-allocation hook, and that is
`allocator_api` — unstable, so a codec built on it builds only on nightly, and a
library nobody can ship on stable is not a library. A `#[global_allocator]` is
worse: process-wide, so it cannot be scoped to a decoder, and it forces `unsafe`
into a crate that is `forbid(unsafe_code)`.

Buffer donation is the stable answer, and it is the one libzstd reaches for as
well (`ZSTD_initStaticDCtx` and its workspace). L1 already spoke the currency —
`Buffers`, `take_buffers`, `donate_buffers` — so L2 only had to decide where
buffers come from and where they go: `BufferSource` is the trait a host
implements over an arena, a slab or a `mach_vm` region, and `Recycler` is the
built-in bounded free list.

Two things this had to get right.

**Retained buffers are charged.** A free list nothing accounts for makes the
budget lie by exactly the size of the free list — which is the same mistake as
not accounting at all, one level down. A `Recycler::budgeted` holds its own
`Lease`, charging on retain and refunding on hand-out, and **when the budget
refuses the charge it drops the buffers rather than retaining them**. A recycler
can never push the process over its ceiling; the worst it can do is fail to help.

**Reset refills, and order matters.** The pool's `give_back` originally shrank
and then reset. With a source that is a no-op loop: shrink hands the buffers to
the free list and reset takes them straight back, so nothing is ever shrunk.
Resetting first and shrinking second is correct, and a shrunk decoder gets its
buffers again on checkout — which is the *point*, since the alternative is
re-faulting pages that existed moments ago.

The security property that buffer reuse introduces is pinned directly:
`a_recycled_buffer_never_leaks_the_previous_response` decodes a distinctive
payload, returns the buffers to the free list, and asserts none of it appears in
the next response's output. Capacity is donated; contents never are. This holds
by construction today — `forbid(unsafe_code)` means nothing can read past a
buffer's length — which is precisely why it is worth a test before an
accelerated path starts making assumptions about pre-filled memory.

#### What the CLI is for

`zalt(1)` is the first consumer of `zalt-embed` that is not a test, and that is
most of its value. A host has to hold the streaming API a particular way — feed
socket-sized chunks, drain output as it appears, never require the whole
response in memory — and an API awkward for that will be awkward for a browser.
`--in-buf 1` decodes byte-by-byte through every header field and bitstream
boundary there is, and the integration tests assert output is identical at eight
chunk sizes.

It also makes the policy surface *visible*. The window cap, the memory budget
and the buffer recycler are the interesting parts of L2 and are otherwise
unreachable outside a test; here they are flags, so they can be demonstrated and
deliberately misused. `zalt check` over `golden-decompression-errors/` is a
one-line way to ask the question that matters — is this decoder too permissive —
and `zalt info` walks frame and block structure without decoding, which is what
one actually wants open beside RFC 8878.

Zero dependencies, so the argument parser is hand-written. A CLI with four flags
does not need a derive macro and a dependency tree.

**Known gap.** The C ABI exposes `zalt_decoder_footprint` but no `Budget`,
`DecoderPool` or `BufferSource` — a C host can *observe* what a decoder holds
and account for it, but cannot hand zalt a ceiling or an arena. That matches the
boundary's stated philosophy for threading ("the host already has a model and
does not need ours"), and it is defensible for footprint too. It is recorded
here rather than assumed, because a Mac app embedding this in-process is exactly
the host the allocator seam was built for, and it currently cannot reach it.

### M3 — Apple Silicon — in progress
- [x] `zalt-neon`: wildcopy, `vqtbl1q_u8` small-offset overlap (M0).
- [x] Word-at-a-time bit reader with a cached container.
- [x] Wrapping match copy in flat runs, reaching the kernels.
- [x] 4-stream Huffman ILP interleaving.
- [x] Inline small-copy paths for matches and literals.
- [x] Fused FSE lookups and combined bit reads in the sequence loop.
- [x] Zero-copy `Sink` — no throughput gain, but no forced caller buffer.
- [x] `zalt-bench decode` — end-to-end throughput, and a timing core that
      survives a laptop.
- [x] Harness corrected (seventh pass): corpus frames built with `--no-check` to
      match what the library emits, and the quiet-machine check given an
      absolute swap floor. **Both change reported figures without changing the
      decoder** — see the seventh pass before comparing across them.
- [~] Fuse sequence decode with execution — tried twice, reverted twice. The
      re-run owed by the tenth pass is done (twelfth pass): **6.4% slower** on
      real `.zdoc`, gap 2.88x -> 3.09x, alternating binaries with zstd in every
      round. Settled; do not try a third time without a new argument.
- [~] FSE 3-state interleaving — largely already in place.
- [~] Double-symbol Huffman tables — implemented, 8.4% slower on real `.zdoc`,
      reverted. Conflicts with the four-stream interleave.
- [x] Per-block FSE table clones removed (+9.6% on real `.zdoc`).
- [x] Checksum skipped when the frame declares none (8% on a quiet machine; the
      ~26% first recorded was memory pressure — see the seventh pass).
- [~] Match-source prefetch — tried, 3.5% slower, struck. (NEON XXH64 struck
      too — see §5.)
- [x] `backend_equivalence` fuzzing clean — 26,916 runs under ASan.
- [~] `zalt-darwin` — profiling does not justify it; M2's pool captures the
      memory cost portably. Not built.

**Where it stands.** Figures below were taken on a **busy** machine and are
lower bounds, *except* the standing-numbers table, which was re-taken on a quiet
one in the seventh pass. Two harness corrections landed there as well, and both
move reported figures without touching the decoder — read the seventh pass
before comparing anything in this section against a fresh run.

On 35 MB of varied text at `zstd -3`, the starting point was 99 MB/s. On the
current 60 MB corpus the same path reaches **640 MB/s** through a zero-copy
sink. The 640 has since been confirmed on a quiet machine and is *not* a floor;
the **1400 MB/s** reference it was compared against was, and the gap is **2.7x**
rather than the 2.2x reported here. See the seventh pass.

Two caveats on that comparison. The corpora differ, so the 99 → 640 figures are
not a single controlled series; the trustworthy numbers are the per-change A/B
results below. And a large part of the apparent gain came from *fixing the
harness* rather than the decoder — measuring a reused decoder instead of
allocating one per iteration moved the number by roughly 45% on its own.

#### The benchmark had to be fixed first, and that was not a formality

The first `decode` run said NEON was **0.62x of scalar** — that the accelerated
backend made whole-frame decoding *slower*. Taken at face value it would have
inverted the entire L1a premise.

It was noise. Three consecutive runs of the same binary on the same input gave
49.5, 86.3 and 88.3 MB/s, and the NEON ratio ranged 0.58x to 1.63x. The
estimator took a single timing run per shape, and a single run on a laptop
measures the machine as much as the code.

The fix is that interference is **one-sided**. Another process, a P-to-E core
migration, a thermal step, a page fault — each can only make a sample slower;
nothing makes one spuriously fast. So the *fastest* of nine batches is the
closest estimate of what the code costs, and a mean is an average of the code
plus however busy the machine was. Spread is reported alongside, and a row whose
best and worst batch differ by more than 15% is printed `~noisy` so it cannot be
mistaken for a precise number.

This is worth recording because the same trap caught the same work twice: the
4-stream Huffman interleaving *also* first measured as a regression, and was
also a false alarm — that time from a `cargo build` running concurrently with
the benchmark. Both would have been acted on. **Re-run before believing a
result, and A/B two binaries back to back rather than across edits.**

#### Profile first: the ranked list was right about the mechanism, wrong about the file

§5 ranks 4-stream Huffman decode as the largest single win. On this corpus it
was worth ~5%, and the two changes that mattered were not on the list at all.
The ranking was derived from architecture rather than measurement, which the
section says of itself — this is what that caveat cost.

Leaf-weighted samples, before and after:

| | before | after |
|---|---:|---:|
| sequence execution (`copy_match`) | 51% | 5% |
| FSE sequence decode | 41% | 23% |
| Huffman literals | 9% | 7% |
| output copy (`memmove`) | 4% | 24% |

**The accelerated kernel was unreachable.** `Window::copy_match` was half of all
decode time, and the NEON and scalar builds spent *identically* as long in it —
3264 against 3175 samples. The reason is in this document: the ring's wrapping
path was byte-at-a-time with an integer division per byte, justified on the
grounds that "a frame smaller than its window never wraps". True, and irrelevant
for anything large — the benchmark frame declares a 2 MiB window and 33.8 MiB of
content, so it wraps after 6% of its life and spends the other 94% on the slow
path, never once reaching a kernel. Splitting a wrapping match into flat runs
(three enumerated cases, each bounded by hand) took it to 5% and is what finally
made NEON visible end to end.

So: **the NEON kernels were never the problem, and tuning them further would
have bought nothing.** They were fine; the code around them denied them the work.

#### What each change bought

| change | | measured |
|---|---|---:|
| Word-at-a-time backward bit reader | 99 → 140 MB/s | +41% |
| Wrapping match copy in flat runs | 140 → 210 MB/s | +50% |
| Bounded output pre-reservation | 210 → 213 MB/s | +1% |
| 4-stream Huffman ILP + cached bit container | 213 → 216 MB/s | +5%* |

\* A/B of two binaries alternating on an idle machine: 211/213/216 interleaved
against 188/205/206 sequential. Measured across edits it looked like a
regression twice.

#### Second pass: the per-sequence cost

With the ranked list exhausted, the profile pointed somewhere it had not:
`memmove` at 26%. The first guess — the window-to-caller drain copy — was
wrong, and measurably so.

| change | | measured |
|---|---|---:|
| Zero-copy `Sink` (removes the drain copy) | 216 → 216 | **0%** |
| Inline small match copy (`copy_small`) | 216 → 245 | +16% |
| FSE: one table lookup per state, masked | 245 → 251 | +2% |
| One combined read for three state advances | 251 → 279 | +11% |
| One combined read for three value fields | 279 → 299 | +8% |
| Literal writes through the backend, inline small copy | 299 → 304 | +2% |
| Hoist the per-sequence limit checks | 304 → 317 | +5% |
| xxh64: borrow each stripe instead of copying it | 317 → 321 | **0%** |

(`Vec`-returning path; the sink path tracks about 5% above it.)

**Two of these bought nothing, and both were predicted to be large.** The drain
copy was estimated at 20% and measured at zero: a 35 MB `memcpy` at memory
bandwidth is about 1 ms against a 105 ms decode. The xxh64 stripe copy was a
genuine redundancy — every byte of output moved once more for no reason — and
LLVM was already eliding it.

What `memmove` actually was: **thousands of tiny calls**, not one big one. A
literal run between two matches is a handful of bytes, and both `copy_within`
and `copy_from_slice` lower to a libc call that must branch on a length it
cannot see before moving anything. Below about 32 bytes the call costs more
than the copy. Two fixed-width moves anchored to the head and the tail of the
range cover any length in between, write nothing outside it — so no slack is
needed past the destination — and are a single load and store each. That one
change was worth more than everything on §5's ranked list combined.

The other theme is that **the sequence loop was doing the same work repeatedly**.
It fetched each FSE entry twice (once for the symbol, once to advance out of the
same unchanged state), took six separate bit reads where the format lays the
fields out consecutively so three reads suffice, and recomputed
`block_size_max`, `window.total()` and two `Option`s twice per sequence for a
ceiling that cannot move during a block. None of that is clever; it is the cost
of writing the correct version first, which was the right order to write it in.

#### Third pass: three attempts, one kept

| change | measured |
|---|---|
| Fuse sequence decode with execution | **~4% slower** — reverted |
| Precomputed shuffle tables in the small-offset kernel | +1% — kept |
| Flatten the LL/ML code `match` into a lookup table | **9% slower** — reverted |

Two of three made it worse, and the two that did were both "obvious"
optimisations.

**Fusing decode with execution.** The `Vec<Sequence>` a block fills and then
walks is ~70 MB of traffic against 35 MB of output, so removing it looked
free. It cost about 4%, and `#[inline(always)]` on the executor recovered only
part of that. The merged loop has to hold three FSE states, three table
entries, a bit-reader and the window's cursor at once; the separate loops each
fit. Sequential bulk traffic is cheap and register pressure is not — which is
the **third** time in this section that an estimate of memory-traffic cost was
too high, after the drain copy and the xxh64 stripe.

> **Re-run this one.** The tenth pass profiles libzstd and finds it does exactly
> the fused thing, in a single function that is 87% of its code-corpus time and
> 52% on a real `.zdoc`. The 4% above is, by this section's own admission, its
> weakest measurement — two builds at different times rather than alternating
> binaries — so it is the reverted change whose verdict is least safe to trust.
> Register pressure may still turn out to be the answer; the point is that it
> has not been measured against the current decoder, and the reference's profile
> says this is where its advantage lives.

**Flattening the code tables.** `literals_length_code` reads as a range
`match` that should obviously be an array index. As a `match`, the low codes —
the common ones — resolve in a compare and a move with no memory access at all.
As a table, every sequence pays a load. 9% slower, consistently.

The kernel change is the only one that stuck, and it is small: two 16-byte
shuffle index tables were being built with a modulo per byte on every call,
thirty-two integer divisions before a byte moved, and the sub-vector tail was a
byte loop. Both are now `const` tables and a fixed-width store. It is worth
only ~1% here because most matches on this corpus have offsets past 16 and
never reach that kernel; it should matter more on markup, where indentation
runs and repeated tags produce exactly the short offsets it exists for.

#### Fourth pass: the checksum nobody asked for

With the harness no longer measuring `malloc`, XXH64 went from 3.5% of the
profile to **7.3%** — it had been hiding behind the allocator. Two changes
followed, and only one of them did anything.

**Hashing was not gated on the frame declaring a checksum.** Every byte of
output was hashed whether or not anything would ever compare the result. On a
`--no-check` frame, skipping it measured **~26%** here — more than the 7% the
profile suggested, and the explanation offered was that not hashing also avoids
pulling all of the output back through the cache.

**That 26% did not survive a quiet machine; the real figure is 8%.** The
explanation is what gives the correction away: pulling output back through the
cache is only expensive when something else is contending for it, so the
number was largely measuring the memory compressor. The change is still worth
making — 8% for a branch is a good trade, and it is free on frames that declare
no checksum, which §10 notes is what server-side library callers actually emit.
But the reasoning was right in a way that should have predicted its own
fragility, and did not. See the seventh pass.

That is not a niche case. The zstd *CLI* writes a checksum by default, which is
why every frame in this document's benchmarks has one; but `ZSTD_c_checksumFlag`
in the **library** defaults to off, so frames produced server-side by
application code — which is what a browser actually receives — typically carry
none. The measurement corpus is the unrepresentative one here.

**Hoisting the accumulators into locals did nothing** (641 against 640,
alternating binaries). The four chains are independent and were already being
promoted out of `&mut self` by the optimiser.

§5's ranked item #6 is "XXH64 with NEON. Four 64-bit accumulators map to two Q
registers." That will not work: **AArch64 NEON has no 64-bit integer
multiply** — `MUL` covers 8, 16 and 32-bit elements only — so a vector XXH64
must emulate each product from 32-bit widening multiplies and reassemble the
carries, which is slower than the scalar `mul` Firestorm already issues two of
per cycle. The item should be struck rather than attempted; it is the fourth
entry on that list to fail on contact with a measurement, and the second to
fail on an architectural fact rather than a surprise.

#### Fifth pass: prefetch, and the shape of this whole exercise

Match-source prefetch was the last untried item on §5's ranked list. It cost
3.5%, isolated A/B, and is struck with the reasoning recorded there.

That makes the tally for the ranked list of "wins" this document opened with:

| § 5 item | outcome |
|---|---|
| #1 4-stream Huffman ILP | +5%, kept |
| #2 small-offset overlap kernel | already present from M0; +1% more from precomputing its tables |
| #3 bulk wildcopy | correctly predicted to yield little — but the *small*-copy case it did not consider was the single largest win of M3 |
| #4 FSE state interleaving | largely subsumed by fusing the lookups and combining the reads |
| #5 double-symbol Huffman | untried; Huffman is now under 3% on code-shaped input |
| #6 NEON XXH64 | struck — the instruction does not exist |
| #7 match-source prefetch | struck — 3.5% slower |

Two of seven were worth what they claimed. The changes that actually moved the
number — a word-at-a-time bit reader, a wrapping match copy that reached the
kernels at all, inline small copies, hoisted per-sequence checks, and not
computing an unwanted checksum — appear nowhere on the list, because the list
ranked *kernels* and the time was going to the code around them.

The list was written from architecture without a profile, and says so. This is
what that costs, and the correction is cheap: **profile before ranking.**

#### Sixth pass: measuring the workload this is actually for

Seven real `.zdoc` documents arrived, and they change the picture more than any
optimisation in this section.

| corpus | size | ratio | sink MB/s |
|---|---|---:|---:|
| `corpus/varied.txt` (code) | 61 MB | 12.0x | ~640 |
| `synthetic.zdoc` | 219 KB | 12.1x | ~556 |
| real `.zdoc` | 3.4–11.8 KB each | 1.9–2.8x | **170–216** |

**Real documents decode at a third of the throughput of large files**, and that
is the workload: §8's push model is many small resources, not a few big ones.
Per-frame fixed cost dominates, exactly as §7 said it would.

Where it goes, on a 7.8 KB document:

| | code corpus | real `.zdoc` |
|---|---:|---:|
| `huf::decode_four` | 2.7% | **15.5%** |
| `memset` (table zeroing) | 1.8% | **6.9%** |
| `DecodeTable::build` | 1.5% | **4.3%** |
| `Distribution::parse` + `from_weights` | ~1% | **6.8%** |

Entropy table setup is ~18% before decoding a byte. This is the cost §7 says
dictionaries remove, because a dictionary carries the tables pre-built — and it
is the strongest argument in this document for prioritising M4. It is *not* a
complete answer: `decode_four`'s 15.5% is real decoding, not setup, and §7's
adoption caveat means the no-dictionary path stays the wire reality until `dcz`
negotiation exists.

**One prediction in §8 did not survive contact.** It expects `.zdoc` to be
literal-dominated and match-poor — "a reference-dense `.zdoc` has a large
incompressible fraction no match finder can touch," from mandatory SHA-256
hashes, ed25519 signatures and `urn:uuid` identifiers. Across 264 KB of real
documents those tokens are **0.8% of the bytes**: 4 hashes, 18 base64 runs, 14
UUIDs. The documents are structural markup, and their 1.9–2.8x ratios reflect
that. §8's corpus table should be re-derived from real traffic before it drives
a recipe.

**What that pass bought.** `resolve_table` built each of a block's three FSE
tables, returned it *by value*, and then **cloned** it into the slot that
carries it across blocks — and `Repeat` mode cloned a table it already had. A
`DecodeTable` is a fixed 512-entry array, so that was ~2 KB per table and 6 KB
per block. Invisible on a 60 MB frame; on a 7.8 KB document it is a large
fraction of the work. Writing into the slot and borrowing from it is **+9.6% on
real `.zdoc`** and +1% on the code corpus.

**Double-symbol Huffman, tried and rejected.** §5's item #5, and on this
workload the one item whose target was genuinely large — 15.5%. Implemented by
deriving the paired table from the single-symbol one (for a window `w`, the
second symbol's bits are all real exactly when `a.nb_bits + b.nb_bits <=
table_log`, which is both the pairing rule and its validity test). It measured
**8.4% slower** on real `.zdoc` and 2% slower on the code corpus.

Two reasons, and the second is the one worth keeping. Building the paired table
costs `1 << table_log` iterations *per frame*, landing on precisely the
workload that can least afford per-frame cost. But the larger problem is that
**pairing and the four-stream interleave conflict**: streams emitting one or
two symbols per step advance at different rates, so they can no longer share an
index, and the natural implementation walks them one at a time — trading the
four independent dependency chains that won +5% for a single chain. Keeping
both would mean four cursors stepped in lockstep with per-stream bounds checks,
for a gain that the build cost already eats into. Reverted.

**`zalt-darwin` is not justified.** The checklist says "if profiling justifies
it — not before", and it does not: no OS or allocator symbol appears anywhere
in the top of either profile. Memory management *did* prove to matter — reusing
a decoder rather than allocating one per frame was worth ~45% — but M2's
`Recycler` and pool already capture that portably, which is a better outcome
than a `mach_vm` dependency.

#### Seventh pass: the quiet machine, and what it corrected

The first session to measure after a restart, with nothing else running. It
changed no code. Three of the numbers this section had been quoting were wrong,
and **all three were wrong in the direction that flattered us** — which is the
finding, because none of them was a coding error and each had a plausible
reason recorded beside it.

**What held.** `sink` at 640 MB/s on `corpus/varied.txt`, reproduced to within
0.8%. The real `.zdoc` documents at 175–220 MB/s against the 170–216 recorded.
`just gate` green at 290 tests. The corpus survived the reboot as designed.

**What did not, and why.**

| claim | recorded | quiet machine | why it was wrong |
|---|---:|---:|---|
| `zstd -t` reference | 1400 | **1169** | never reproduced; and `-t` is the wrong shape regardless |
| checksum skip | ~26% | **8%** | the 26% was mostly the memory compressor |
| `corpus/ztml` = "3–12 KB documents" | ~434 | **209** | 83% of the corpus by bytes is one 219 KB synthetic file |

The third is the one to take seriously, because it is a *labelling* error rather
than a measurement error and it has been steering the roadmap. `just measure
corpus/ztml` time-weights its files, correctly — but `synthetic.zdoc` is 219 KB
of the corpus's 264 KB and decodes at 558 MB/s, so it supplies most of the
weight. The aggregate is a number about the synthetic file. The six real
documents, measured alone, are **209 MB/s time-weighted** against **504 MB/s**
for `zstd -b3` on the same six: a **2.41x** gap on the workload §8 says is the
actual target.

| what | ratio | zalt sink | zstd `-b3` | gap |
|---|---:|---:|---:|---:|
| `corpus/varied.txt`, 61 MB | 12.0x | 640 · **686** no-check | 1964 | 2.86x* |
| 6 real `.zdoc`, 3.4–11.8 KB | 2.48x | 209 | 504 | 2.41x† |
| `synthetic.zdoc`, 219 KB | 12.1x | 558 | — | — |

\* matched, and the 686 is the number in the ratio: `-b3` never checksums, so
the comparable zalt figure is the no-checksum one from the isolated flag A/B.
The 640 beside it is the checksummed `just measure` result and is **not**
comparable to the reference in the same row.

† *not* matched. zalt checksums here and `-b3` does not, so the real gap on the
target workload is somewhat under 2.41x. This is the row most worth re-taking
now that the harness passes `--no-check`.

**Interference is one-sided — but only for the code, not for the comparison.**
This section's governing rule is sound and was applied too broadly. A busy
machine can only make *zalt* look slower, so zalt's figures really were floors.
It can only make the *reference* look slower too — so quoting a reference taken
under load does not produce a conservative gap, it produces a flattering one.
Every number in a ratio needs the discipline, not just the numerator.

**The quiet-machine warning was itself broken, and failed silently.** It fired
on every run of this session, at loads as low as 1.30 on a machine doing
nothing. The swap term was `used < 5% of total`, and Darwin sizes the swap file
dynamically — so `total` is a measure of past pressure, about 1 GB after a
reboot against the 12 GB this laptop reaches after a long session. 56 MB of
residual swap is 5.5% of a fresh file and fails; 600 MB of a grown one passes.
**The check got stricter as the machine got cleaner**, so it was loudest at
exactly the moment it was meant to certify, and a session that did the right
thing was told its numbers were floors anyway. Fixed with an absolute floor
alongside the share, and pinned by four tests in `zalt-bench` — the first that
crate has had.

**The harness now compresses with `--no-check`, and this breaks the series.**
The corpus was being built by the zstd *CLI*, which writes a content checksum
by default; the *library* does not, so frames produced server-side by
application code — what a browser actually receives — typically carry none.
Every figure recorded in this section therefore charged zalt for an XXH64 pass
the deployed workload does not pay, on a corpus chosen to represent that
workload. `compress_bytes` now passes `--no-check`, which also matches what
`zstd -b` benchmarks and removes the correction the comparison needed.

**Consequence, and it is a trap: every number above this paragraph was taken
with checksummed frames, and every number below will not be.** On
`corpus/varied.txt` that is a step of about **8%** — 640 becomes roughly 690 —
caused by a flag and by no change to the decoder whatsoever. Anyone A/B-ing a
new build against a figure recorded here will read that as an 8% improvement
they did not make. This is the same "two runs at different times are not
comparable" hazard this section keeps rediscovering, in its most durable form
yet, because the discontinuity is now baked into the tool rather than into the
weather. **Re-baseline before trusting any comparison across it**, and note that
the change has not been re-measured — the 8% is the isolated A/B of the flag,
not a fresh corpus run.

**What this changes about what to do next.** The gap on the target workload is
2.41x, and §10's own profile of a real document puts ~18% in entropy table
construction before a content byte is touched. That share is not reachable by
the unsafe seam, which targets inner loops that small documents barely enter;
it is exactly what a dictionary removes. **M4 addresses a larger share of the
target workload than the unsafe seam does**, and it does so without spending
the security claim. §7's adoption caveat is untouched by any of this and
remains the reason M4 is not a free win.

> **Corrected by the eighth pass.** The first two sentences hold — the 18.7% is
> real and is not reachable by unsafe. The conclusion does not: a profile taken
> immediately afterwards puts **48%** of real-`.zdoc` time in bounds-checked
> inner loops, the same share as on the code corpus. Small documents do not
> "barely enter" them. M4 and the unsafe seam are complementary, and this
> paragraph reached a comparative verdict from one side's number alone.

#### Eighth pass: the post-harness baseline, and the profile that corrects the plan

Taken on a quiet machine with the corrected harness (`--no-check` frames, swap
floor). **This is the reference point for the unsafe-seam experiment**; the
tables above it are the old, checksummed series and are not comparable.

| workload | scalar | neon | **sink** | `zstd -b3` | gap |
|---|---:|---:|---:|---:|---:|
| `corpus/varied.txt`, 61 MB | 498.7 | 643.5 | **698.2** | 1965.6 | **2.82x** |
| 6 real `.zdoc`, time-weighted | — | — | **213.8** | 503.5 | **2.36x** |
| `synthetic.zdoc`, 219 KB | 526.8 | 581.7 | **594.6** | — | — |

Per-file real `.zdoc` sink runs 179–227 MB/s. The `--no-check` change moved
`sink` from 639.8 to 698.2 (+9.1%) on the code corpus and 209.1 to 213.8 (+2.2%)
on real documents — larger where there is more output to hash, as expected, and
in both cases from a flag rather than from the decoder. The references reproduce
to within 0.2% across sessions, which is worth knowing: `-b3` is a far steadier
instrument than `zstd -t` was.

**The fresh profile, and it overturns the reason given for preferring M4.**
Leaf-weighted, harness timer excluded, categories aggregated:

| | real `.zdoc` | code corpus |
|---|---:|---:|
| match / literal copy | 29.5% | 45.2% |
| sequence decode (FSE) | 22.6% | 26.1% |
| **entropy table setup** | **18.7%** | 6.3% |
| Huffman decode | 13.9% | 2.6% |
| bit reads | 11.8% | 19.8% |
| allocator | 2.8% | 0.1% |

The 18.7% confirms the sixth pass's table-setup finding exactly, and it remains
dictionary-addressable and *not* reachable by unsafe — §6 places
`forbid(unsafe_code)` on table construction, which is precisely this code.

But the seventh pass concluded from that number that the unsafe seam "targets
inner loops that small documents barely enter", and **that is wrong**. Summing
the categories where bounds-checked indexing actually lives — sequence decode,
Huffman decode and bit reads — gives **48.3% on real `.zdoc` and 48.5% on the
code corpus**. The surface is the same size on both workloads; small documents
enter those loops as much as large ones do, they simply also pay a table-setup
cost that large ones amortise away.

So M4 and the unsafe seam are **complementary, not competing**, and the earlier
framing of them as alternatives was an artefact of reading one number without
its complement. (The ninth pass then found that most of that 48% carries no
bounds check at all, and the tenth that table setup is the area where zalt is
*closest* to the reference — so neither of these two paragraphs survives intact.
Read them as the reasoning, and the two passes that follow as the result.) Two further cautions on the 48%: bounds checks are only a
fraction of what those loops cost, so the realisable gain is a fraction of 48%,
not 48%; and the largest category on both workloads — match and literal copy —
already runs through `zalt-neon`'s kernels and is *already* unsafe, so it is not
additional surface.

**Where to put the unsafe, in profile order.** Highest value first, and each is
independently revertible:

1. **FSE table lookup in `sequences::decode`** — 22.6% / 26.1%, the largest
   safe-code category on both workloads, and the index is already masked to
   `1 << accuracy_log` by `entry_masked`, so the bound is established by
   construction rather than by the check being removed.
2. **`huf::Table::decode_four`** — 13.9% on real `.zdoc` against 2.6% on the
   code corpus. Small-document-specific, and the four-stream interleave means
   four independent bounds checks per step.
3. **`BackwardBits`** — 11.8% / 19.8%, though this reader is already
   word-at-a-time, so what remains is the cursor check rather than a per-bit one.

#### Ninth pass: the unsafe seam's premise, tested — and unsupported

This section has said for some time that the remaining gap is bounds checks:
*"libzstd's inner loops use raw pointers with no bounds checks... ours
compare-and-branch on every slice access, several times per sequence"*, and
therefore **"parity is not reachable without it."** That claim was never
checked. It was tested on a branch before any `unsafe` was written, and it does
not hold.

The method was to disassemble the release binary and count bounds-check call
sites (`panic_bounds_check` and friends) per function, then remove some and
measure.

| function | profile share (`.zdoc`/corpus) | instrs | panic sites |
|---|---|---:|---:|
| `sequences::decode` | 22.6% / 26.1% | 422 | **0** |
| `BackwardBits::read` | part of 11.8% / 19.8% | 42 | **0** |
| `BackwardBits::read_wide` | ” | 41 | **0** |
| `huf::Table::decode_four` | 13.9% / 2.6% | 913 | 9 |
| `Decoder::execute` | part of 29.5% / 45.2% | 350 | 5 |

**Two of the three planned targets have no checks to remove.** `entry_masked`
indexes a fixed `[Entry; 512]` with a masked index, and LLVM already proves it
in range. Confirmed by trying the safe version of the optimisation first —
masking additionally by the constant `TABLE_SIZE_MAX - 1`, which gives the
optimiser a static bound without any `unsafe`. Alternating binaries, best of
four: **455.8 before, 453.9 after.** Nothing, because there was nothing there.

**The third target had checks, and removing them was free.** `decode_four`
carries a comment claiming its mask already removes the bounds check. It does
not: `mask` is `entries.len() - 1`, which underflows to `usize::MAX` on an empty
`Vec`, so the compiler cannot conclude `i & mask < len` and kept a check on
every one of the four streams. Establishing `len > 0` once, outside the loop —
again with no `unsafe` — removed **4 of the 9 sites and 32 instructions**.
Measured, alternating, best of five: **458.1 before, 458.4 after.** Dead even.

So on the ~48% of decode time the eighth pass identified as "bounds-checked
inner loops", either there is no check or removing it is free. These are
perfectly predicted branches on an eight-wide out-of-order core, and the loops
are limited by the serial dependency chain through the bit reader, not by
instruction count. **The premise that bounds checks are the gap is unsupported,
and spending `forbid(unsafe_code)` to remove them would buy nothing measurable.**

What this does *not* establish: a full raw-pointer rewrite was not tried, and
§10's other claim — libzstd's "unaligned 64-bit loads throughout" — is a
structural difference rather than a check, untouched by this experiment.
`Decoder::execute`'s five sites were not isolated. The narrow finding is that
*bounds-check removal specifically* is not where the time is.

#### Tenth pass: where the reference's advantage actually is

Rather than infer it again, libzstd was profiled directly. The harness is
`crates/zalt-bench/reference/zstd-profile.c` (`just profile-zstd`), which links
`libzstd.a` and mirrors `zalt-bench profile` exactly — context reused across
iterations, input already in memory, output into a preallocated buffer, no I/O
and no allocation inside the loop. It reproduces `zstd -b3` to within 3% on both
corpora, which is what makes it trustworthy.

| category | zstd % | zalt % | zstd | zalt | **ratio** |
|---|---:|---:|---:|---:|---:|
| **61 MB code corpus** | | | 32.3 ms | 95.9 ms | **2.97x** |
| sequence decode + execute | 87.0% | 91.1% | 28.1 ms | 87.4 ms | **3.11x** |
| Huffman decode | 3.4% | 2.6% | 1.10 ms | 2.49 ms | 2.27x |
| entropy table setup | 9.6% | 6.3% | 3.08 ms | 6.04 ms | 1.96x |
| **7.8 KB `.zdoc`** | | | 15.5 µs | 38.9 µs | **2.50x** |
| sequence decode + execute | 52.0% | 63.9% | 8.08 µs | 24.86 µs | **3.08x** |
| Huffman decode | 19.8% | 13.9% | 3.07 µs | 5.41 µs | 1.76x |
| entropy table setup | 28.2% | 18.7% | 4.38 µs | 7.28 µs | 1.66x |

**The gap is one path.** Sequence decode and execution is 3.1x on *both*
workloads — the same figure on a 61 MB file and a 7.8 KB one — and it is 64–91%
of zalt's time. Everything else is 1.7–2.3x. Bring that one path to parity and
zalt lands near 1.3x overall.

**Entropy table setup is zalt's best area, not its worst**, at 1.66x, and
libzstd spends a *larger share* of its time there than zalt does (28.2% against
18.7%). This weakens the eighth pass's argument for M4 as a gap-closer: a
dictionary still removes ~18.7% of small-document time, worth a real 1.23x on
latency, but it attacks the one place zalt is already competitive and libzstd
would gain proportionally more from the same change. **M4 is a latency
improvement, not an answer to the 3x.**

Two struck items are independently confirmed by the reference's own profile.
`HUF_decompress4X2` — double-symbol Huffman, §5 item #5 — is 0.5% of libzstd's
code-corpus profile and *absent* on the `.zdoc`, where it uses the
single-symbol `4X1`. And `ZSTD_decompressSequencesLong`, the prefetching
variant, is 25 samples of 9182. Both were reverted here after measuring worse;
the reference barely uses either.

**What is structurally different.** `ZSTD_decompressSequences` is one fused
function: FSE state machine, bit reader and `ZSTD_execSequence` all inlined into
a single loop with everything in registers. zalt splits the same work into
`sequences::decode` filling a `Vec<Sequence>`, `execute` walking it, and the
copy kernels — three passes over a materialised intermediate. That is precisely
the fusion tried and reverted in the third pass at "~4% slower", **the number
this section already identifies as its weakest**, because it was taken across
two builds at different times rather than by alternating binaries. Re-running it
under the eighth pass's baseline is the highest-value experiment available, and
it requires no `unsafe`.

A second candidate inside the same path: libzstd's `execSequence` wildcopies 16
bytes at a time, overrunning into output slack, and falls back to
`ZSTD_safecopy` only near the buffer end — which appears at 17 samples,
confirming the overrunning path is the common one. zalt copies exact lengths.
§5 anticipated this ("overrunning into padded output slack"); whether that slack
exists in the window is worth checking.

**Caveat on resolution.** libzstd's fusion is also a measurement limit: because
one symbol contains FSE decode, bit reads and match copy together, a leaf
profile cannot decompose the 3.1x among them. "The sequence and execute path is
3.1x" is supported; attributing it further is not, and would need
instruction-level sampling.

#### Eleventh pass: acting on the structure, and a control that caught a 55% swing

The tenth pass found libzstd doing the whole sequence loop in one function while
zalt spreads it over five. This is the first change made on that finding, and
the first thing in three passes to actually move the number.

**`#[inline(always)]` on `Window::write_all` and `Window::copy_match`.** Both
live in the same crate as `execute`, so LLVM could always see their bodies and
was *choosing* not to inline them on cost heuristics. The profiler is what
proved they were real calls rather than a reading artefact: `sample` attributed
separate leaf time to each one, which it cannot do to something that was
inlined.

The disassembly confirms the intended effect. `Window::write_all` disappears as
a symbol entirely, `Decoder::execute` grows from 350 to 584 instructions, and
`Backend::copy_match` stops being called from `execute` — the whole per-sequence
chain collapses into one function, which is the shape the reference has.

| | before | after | |
|---|---:|---:|---:|
| real `.zdoc` | 457.6 | 467.3 | **+2.1%** |
| 61 MB corpus | 697.3 | 709.3 | **+1.7%** |
| real `.zdoc` (re-confirmed, different machine state) | 750.8 | 763.2 | +1.7% |
| 61 MB corpus (re-confirmed) | 1118.7 | 1153.3 | **+3.1%** |

Alternating binaries, best of five and best of four. In the first pair the two
distributions did not overlap at all — every "after" round beat every "before"
round — which is worth more than the point estimate.

**One change measured worse and was reverted.** `Window::write_all`'s fast path
is guarded by `is_full()`, so a frame smaller than its declared window — every
real `.zdoc` — fell through to the general loop and reached
`Vec::extend_from_slice`, bypassing the inline small-copy path in
`B::copy_literals` on exactly the workload with the most literal runs per byte.
That looked like a clear miss. Adding a non-full fast path that `resize`s and
then copies measured **5.4% slower**, consistently, across six rounds with no
overlap: the `resize` zero-fills bytes that are immediately overwritten, and the
doubled write costs more than the `memcpy` call it removed. `extend_from_slice`
was already the better answer. Reverted.

That is the **fourth** memory-traffic estimate in this section to come out
backwards, after the drain copy, the xxh64 stripe and the fusion attempt.

#### A canary is worth more than a clean-machine check

The most useful thing to come out of this pass is a measurement technique.

`just bench-zstd` has a **known value** — the reference decoder's throughput on
a given frame does not change when zalt changes. So running it before and after
an A/B turns it into a control: if the canary reads what it read last time, the
machine is in the same state; if it moved, nothing measured across that window
can be compared with anything measured outside it.

It earned its place immediately. Mid-session the canary went from **1905 MB/s to
3058 MB/s — a 55% swing with no code involved**, on a fanless laptop whose
sustained clocks depend on how hot the last ten minutes were. In the same window
the *same* zalt binary read 457 and then 750. Without a control, that 64% would
have been read as a spectacular optimisation; with one, it was visibly the
machine. An A/B taken while the canary drifted (the `before` column moved 534 →
728 *within one run*) was correctly thrown away and re-taken.

The load-average and swap check in `just measure` does not catch this, because
the machine genuinely *was* quiet in both states — it was quiet and slow, then
quiet and fast. **Thermal and frequency state is a third axis, invisible to
`uptime`, and a known-value canary is the only cheap way to see it.**

**It is now wired in.** `just measure` runs the canary before and after the
rounds and prints the drift; above 10% it says the rounds straddle two different
machines and should be re-run. `just canary` reads it on its own.

Three things had to be right for it to be worth having.

**The input is generated, not borrowed.** `ci/gen-canary.py` emits ~8 MB of
seeded markup, seeded for the same reason `gen-text-fixture.py` is: a control
whose input drifts cannot separate "the laptop got slower" from "the input got
harder", which is the only question it exists to answer. It is shaped like
markup so `zstd -3` emits compressed blocks — a canary decoding raw blocks would
measure `memcpy` and would not move when the interesting things move.

**It is a best-of-three, not a single reading.** Consecutive single readings
spread **35%** while the CPU ramped, which would have made the control the
noisiest thing in the room. Best-of-three repeats to a few percent and the cold
first run never wins. The reasoning is the estimator's, one level up:
interference is one-sided, so the fastest sample is the closest estimate — and a
*drifting control is worse than no control*, because it reports its own drift as
the machine's.

**It degrades to silence.** The canary needs the zstd headers; without them
`just measure` prints no canary line rather than failing a measurement over a
missing control.

#### What the canary found within an hour of existing

Two contention sources that `uptime` had not yet reflected, both started by an
editor rather than by anything anyone ran:

- **`cpptools-srv`**, five processes at ~78% of a core each, from the C/C++
  extension indexing `zstd-profile.c` — a *file being added*.
- **`clippy-driver`, seven processes at 24–40% each**, from rust-analyzer
  checking the workspace after a justfile edit. That one matters more, because
  this is a Rust repository and every contributor's editor will do it, on every
  save, forever.

The canary read 481 against a settled 860 while clippy ran — a 44% error, and
the load average had not caught up. Twenty minutes later, quiet, it read 1400:
**the same control, the same input, 1.6x apart**, which is the measurement this
section spent an entire milestone not knowing it needed.

#### Twelfth pass: fusion, re-tried properly, and settled

The tenth pass named this the highest-value experiment available: libzstd
decodes and executes a sequence in one loop, zalt fills a `Vec<Sequence>` and
walks it afterwards, and the −4% that had condemned fusion was the weakest
measurement in this section — two builds at different times rather than
alternating binaries. It was also worth re-trying because the diagnosis then was
register pressure, and the eleventh pass has since inlined `Window::write_all`
and `Window::copy_match` *into* the executor, changing the register situation
the diagnosis rested on.

`sequences::decode` grew a callback form, `decode_with`, and the block path
executed each sequence as it was read. All 294 tests passed in both profiles.

**It is slower, and now by a number worth trusting.**

| | zalt | zstd | gap |
|---|---:|---:|---:|
| two-pass (kept) | 724–730 | 2086–2104 | **2.85–2.92x** |
| fused | 674–684 | ” | **3.05–3.10x** |

Best of eight, alternating, with zstd interleaved in every round. Every single
round favoured the two-pass version and the distributions did not overlap:
**−6.4%**, and the gap against the reference widened from 2.88x to 3.09x. On the
code corpus it was −0.9%, marginal but the same direction. Reverted.

So the original verdict was right, and re-running it was still correct: it was
right for a reason nobody had established, and it is now measured rather than
believed. **The register-pressure explanation survives** — inlining the window
operations into the executor did not free enough room to make the merged loop
fit, and may have made it tighter.

The uncomfortable part is what this does to the tenth pass's conclusion. That
pass found libzstd doing in one function what zalt does in five, and inferred
that the single function was *why* it is faster. Fusing ours makes it slower, so
the inference does not hold: **libzstd is not fast because it is fused; it is
fused and fast.** What the eleventh pass banked (+2–3%) came from removing call
boundaries *within* execution, which is a different thing from merging the two
halves. The structural difference is real and visible in the disassembly; the
causal story attached to it was one step too far.

#### Thirteenth pass: the entropy tables keep their allocations

The best change of the session, and it came from finally measuring *which*
allocation the profile was complaining about rather than inferring it.

`_platform_memset` was 5% of the `.zdoc` profile and `malloc`/`free` another
2.5%. The first guess was the FSE tables, whose `build` zeroes a fixed
512-entry array and returns it by value. Rebuilding those in place cut that
function by 24% and moved the end-to-end number by **nothing** — LLVM was
already eliding the stack array's zeroing, and `memset` did not budge (446 to
438 samples). The change is kept because it does strictly less work and drops an
`Option` indirection, but it earned nothing on the clock.

The memset was `huf::Table`, and the mistake was assuming "per frame":

```rust
let mut entries = vec![Entry::default(); table_size];   // malloc + memset
```

`table_size` is `1 << table_log`, up to 2048 entries, and a **new table is built
for every block that describes one** — not once per frame. `reset_frame_state`
then dropped it wholesale (`self.huffman = None`). So the 61 MB corpus, one
frame of 553 blocks, was allocating and zeroing a table 553 times and freeing
552 of them.

The table now builds in place, grows to fit and never shrinks, and marks itself
unbuilt with `table_log = 0` rather than being dropped — so the `Option`
disappears the same way it did for the FSE tables, and `is_built` is what a
treeless section checks. Everything indexing the table bounds itself by `size()`
rather than `entries.len()`, since the allocation may now outlive a smaller
table.

| | before | after | | gap |
|---|---:|---:|---:|---|
| real `.zdoc` | 719.3 | **752.2** | **+4.6%** | 2.84x → **2.71x** |
| 61 MB corpus | 1089.1 | **1165.7** | **+7.0%** | 2.85x → **2.69x** |

Best of six and best of four, alternating, zstd interleaved in every round;
every single round improved on both the raw figure and the ratio. The code
corpus gains *more* despite entropy setup being only 6.3% of its profile, which
is the per-block detail above showing up: 553 allocations avoided against one.

Two lessons, and the second is the one worth keeping. The obvious one is that a
profile line names a *symbol*, not a cause — `memset` was real and 5%, and the
first thing it was attributed to was innocent. The sharper one is that "per
frame" and "per block" are an order of magnitude apart on a 553-block frame, and
the estimate that made this look like a small `.zdoc`-only win had the wrong one.

#### Fourteenth pass: the bit reader was a function call

The largest single win recorded in this section, and it was sitting in plain
sight behind an `#[inline]` that LLVM declined to honour.

The tenth pass compared zalt's decode path with libzstd's and found one fused
function against five. The eleventh acted on that inside execution and got
2–3%. This pass asked the same question of the *literals* path, and the answer
was blunter:

| | instrs | calls | in the loop |
|---|---:|---:|---|
| `huf::Table::decode_four` | 512 | 19 | **11** |
| `HUF_decompress4X1_usingDTable_internal` | 1307 | 2 | **0** — both setup |

Nine of those eleven were calls to **`BackwardBits::word_at`** — the
eight-byte load the entire word-at-a-time reader is built around, and the
change the twelfth-pass table credits with the largest win of M3's first half.
It carried `#[inline]`; the hint was not taken, and it stayed a real call. Every
symbol decoded therefore spilled the bit accumulator and cursor and reloaded
them, which is precisely the state the design exists to keep in registers.

`#[inline(always)]` on `word_at`, and on `BackwardBits::{read, peek, read_wide,
skip, check}`, takes `decode_four` to 899 instructions with five non-cold calls
and `sequences::decode` to 1049 — the same shape as the reference rather than a
third of its size.

| | before | after | | gap |
|---|---:|---:|---:|---|
| real `.zdoc` | 727.3 | **810.3** | **+11.4%** | 2.85x → **2.56x** |
| 61 MB corpus | 1187.8 | **1225.1** | **+3.1%** | 2.65x → **2.56x** |

Best of six and best of four, alternating, zstd interleaved in every round;
every round improved on both figures, and the within-binary spread was about 1%.
`.zdoc` gains four times what the code corpus does because Huffman literals are
13.9% of its profile against 2.6% — that is where `word_at` was being called
nine times.

**`#[inline]` is a hint, and on the hottest function in the decoder it was
ignored.** Three of this milestone's four wins are now the same mechanism —
removing a call boundary the compiler chose to keep — and in every case the
disassembly said so plainly while the source looked fine. The profile could not
have found this: `sample` attributes leaf time to `word_at`, which is a real cost
correctly reported, and says nothing about the spill around it. Counting calls
in the disassembly and comparing against the reference is what found it, twice.

#### Never quote a raw MB/s — quote the gap

The rule this section has been circling for two milestones, now enforced by
`just gap` and printed by `just measure`.

An absolute figure from this laptop means very little. The same binary has
measured 457 and 750 on the same input; the reference has read 1905 and 3058.
Both swings were thermal, invisible to `uptime`, and larger than every
optimisation in this document put together.

A **ratio measured in the same window** cancels all of it, because whatever
slows zalt slows zstd in the same breath. The difference is stark: across four
consecutive runs the gap repeated as **2.90, 2.88, 2.92, 2.86** — about 1% —
while the figures behind it moved by three times that.

```sh
just gap corpus/ztml     # zalt, zstd, and the ratio, on the same files
```

Both sides must use the same estimator or the ratio is not one: `gap` takes the
best of three from zalt and gives `zstd -b3` matching iterations. A first version
compared a single zalt run against `-i1` and reported 2.43x where the honest
figure was 2.88x.

For an A/B, take **three** readings per round — before, after, and zstd on the
same corpus — and compare the two ratios. That is how the fusion verdict above
was taken, and it is why it is trustworthy where the original was not.

#### The measurement protocol — read this before quoting a number

Everything below is `just measure`. It exists because the figures in this
section were repeatedly wrong in ways that had nothing to do with the code.

```sh
just bench-corpus     # once: builds corpus/varied.txt (survives a reboot)
just measure          # 5 rounds, machine state stamped on the output
just measure corpus 9 # more rounds
```

**Prepare the machine.** Close other editors and build tools — a background
`cargo` moved figures here by up to 58%. Reboot if swap is in use: Darwin's
memory compressor competes for CPU *and* bandwidth, and this laptop has sat at
11 GB of 12 GB swap with 3.2 billion pages compressed. `just measure` prints
the load average and swap usage and warns when they are not clean; believe a
small delta only when it does not warn.

**Then do not measure for ten minutes.** A reboot starts work of its own, and
the last one put `ANECompilerService` — Apple's Neural Engine model compiler,
triggered at login — at 99% of a core for roughly ten minutes. Load average is a
*lagging* indicator and took several minutes more to show it, so the window
right after a restart looks quiet on `uptime` and is not. Check what is actually
running, not just the average:

```sh
ps -Ao %cpu,comm -r | head -5
```

It resolves on its own; nothing needs killing. Spend the wait on the builds —
`cargo build --release -p zalt-bench`, compressing reference frames — so that
the quiet window, once it arrives, holds only measurements.

**An editor with the repo open is a build tool.** Adding
`crates/zalt-bench/reference/zstd-profile.c` woke VS Code's C/C++ extension,
which put **three `cpptools-srv` processes at ~76% each** — later 423% of a core
in total, load 21 — into the middle of a measurement, and depressed the
reference harness from 1905 MB/s to 686. That is a 2.8x error, larger than the
entire gap this section is trying to explain, and it appeared because a *file
was added*, not because anything was run. `just measure` would have caught it;
`bench-zstd` prints no machine state and did not. Check `ps` when a number moves
by more than the change could explain.

**The corpus is `corpus/varied.txt`**, about 60 MB of *distinct* Rust sources.
Distinct matters: an earlier corpus was one file repeated, which at level 3
fits inside the window and therefore measured match copy rather than entropy
decoding. `corpus/` is gitignored and local by design (§5).

**`just measure corpus/ztml` does not report the ZTML number.** Its total is
time-weighted, correctly — and `synthetic.zdoc` is 219 KB of that corpus's
264 KB, so it supplies most of the weight and the total mostly describes it.
For the workload §8 is about, read the per-file rows instead:

```sh
./target/release/zalt-bench decode corpus/ztml 3   # per-file table
```

The six real documents run 175–220 MB/s, 209 time-weighted. The total runs ~435.
Both are true; only one of them is about `.zdoc` traffic.

**The columns.**

| column | what it measures |
|---|---|
| `scalar` / `neon` | one-shot `decompress`: allocates a fresh output `Vec` per frame |
| `sink` | steady-state decode through a reused decoder and a zero-copy `Sink` |

`sink` is the codec; `scalar`/`neon` include the allocator, and under memory
pressure they include the kernel's page reclaim. Reusing the decoder rather
than building one per iteration moved `sink` by roughly 45% on its own — the
harness was measuring `malloc` as much as Huffman.

**The reference — use `zstd -b3`, not `zstd -t`.** This advice has now been
wrong twice in the same direction, and the correction is the same each time:
*the reference must be measured in the same shape as `sink`.*

`sink` decodes from a buffer already in memory, through a decoder it reuses,
writing nowhere. So must the reference. Three candidates on `corpus/varied.txt`:

| how | MB/s | what it also measures |
|---|---:|---|
| `zstd -d -c > /dev/null` | 948 | 60 MB of real `write` syscalls |
| `zstd -t` | 1169 | a process spawn and a file read *per iteration* |
| `zstd -t`, spawn subtracted | 1288 | a file read per iteration |
| **`zstd -b3`** | **1964** | nothing — in-memory, context reused |

The first correction moved from the pipe to test mode, for the right reason.
It stopped one step short: test mode still forks a process and opens a file
every iteration, which on this corpus is **4.83 ms** of the 52.7 ms measured —
about 9%. `zstd -b` is zstd's own benchmark mode, and it is the only one of
these built to measure the codec rather than the CLI.

One trap left in reading `-b` output: its line is `… , C MB/s, D MB/s`, and the
*second* figure is decompression.

The checksum used to be a second trap and is now handled. `-b` frames carry no
content checksum, because `ZSTD_c_checksumFlag` defaults off in the library;
the CLI defaults it on, and the harness builds `corpus/`'s frames by shelling
out to the CLI. The two sides therefore measured different work — worth 16% to
the reference and 8% to us — until `compress_bytes` was changed to pass
`--no-check`. Both sides now decode checksum-free frames and are directly
comparable.

```sh
zstd -b3 -i5 corpus/varied.txt   # second MB/s figure is decode; no checksum
```

**To compare two versions**, none of the above is sufficient. Build both,
keep both binaries, and alternate them:

```sh
cp target/release/zalt-bench /tmp/bench-before   # ... change code ...
cargo build --release -p zalt-bench
for r in 1 2 3; do
  /tmp/bench-before decode corpus 3 | grep ^SUMMARY
  ./target/release/zalt-bench decode corpus 3 | grep ^SUMMARY
done
```

Two runs at different times on this machine are not comparable. A 4%
"regression" and a 9% "improvement" were both recorded that way before this
was understood.

#### Standing numbers, and what they are worth

Re-taken on a **quiet** machine (load 1.3–2.4, 56 MB swap, nothing else
running), best of nine, against the busy figures they replace.

**All of these are checksummed-frame numbers and the harness no longer produces
such frames** — `compress_bytes` passes `--no-check` as of the seventh pass, so
a fresh `just measure` should land about 8% above the quiet column with no code
change behind it. Treat this table as the last of the old series, not as a
baseline to compare a new build against.

| | busy (floor) | **quiet** |
|---|---:|---:|
| zalt, `sink` | 635 | **639.8** |
| zalt, `neon`, one-shot with allocation | 571 | **594.8** |
| zalt, `scalar`, one-shot with allocation | 458 | **471.3** |

**The floors were nearly tight, and which ones moved confirms the model.**
`sink` gained 0.8% — nothing — while the two paths that allocate gained 4.2%
and 2.9%. That is exactly the split this section predicts: only the allocating
paths were ever exposed to page reclaim, so only they were being held down.
`sink` is in-memory and compute-bound, and was never a lower bound at all.

The reference is the figure that was wrong, and it was wrong in the direction
that flattered us — `zstd -t` measured **1169 MB/s**, not 1400, and the 1400 has
not been reproduced. Matched shape and matched checksum, on `corpus/varied.txt`:

| comparison | zalt | zstd | gap |
|---|---:|---:|---:|
| in-memory steady state, no checksum | 686 | 1964 (`-b3`) | **2.86x** |
| in-memory steady state, both checksummed | 635 | ~1689 (derived) | **2.66x** |

So the honest gap on this corpus is **~2.7x**, not 2.2x. Nothing regressed;
the comparison was measuring zalt's in-memory path against zstd's
process-per-iteration path, and the harness overhead was being counted as our
margin.

#### The measurements in this section are lower bounds

Late in the M3 work a second, unrelated build was running on the same machine
for long stretches, and the load average sat between 4 and 22. The estimator
(§10, best of nine batches) absorbs variance *within* a run; it cannot absorb
the machine being in a different state between runs. The same binary on the
same input measured 339 MB/s under load and 535 MB/s when the machine was
briefly idle — a 58% spread, which is larger than every individual change
recorded above.

So: **the absolute figures here are floors, not measurements**, and the
relative ones are only trustworthy where they came from two binaries
alternating under the same conditions. That is how the flat-table and
shuffle-table comparisons were taken. The fusion comparison was not — it was
two builds measured at different times — so its 4% is the weakest number in
this section and worth re-running before treating the design as settled.

The lesson generalises past this project: **A/B two binaries back to back, and
treat any absolute number taken on a shared machine as a lower bound.**

**Since re-taken on a quiet machine** (seventh pass). The floors turned out to
be within 1–4% of the real figures, so the per-change A/B results above stand.
The lesson needs one amendment, though: a lower bound on *our* number is not a
lower bound on a *ratio*. The reference was quoted from the same busy machine
and was understated by more than zalt was, which turned a 2.7x gap into a
reported 2.2x. Apply the discipline to the denominator too.

#### What is left, and whether parity is reachable

Profile after this pass: `sequences::decode` 22%, match copy 16%, literal writes
12%, `execute` 12%, bit reads 17%, Huffman 4%, xxh64 4%. **Superseded — see the
eighth pass**, which re-profiles both workloads on the corrected harness. The
`xxh64` 4% in particular is now zero on the frames the harness builds.

Fusion was tried and reverted. Three-state FSE interleaving is largely already
in place — the loop fetches all three entries before using any of them and
takes their bits as one combined read — so what remains of it is small.
Double-symbol Huffman tables are the one untried item, and Huffman is now under
4% of the profile here; halving it buys under 2%. It would matter considerably
more on literal-dominated content, which §8 predicts ZTML to be and for which
there is not yet a corpus.

That is the honest position: **the safe-code work is close to exhausted.**
Estimates in this section have a poor record, and the remaining items are each
1–2%.

Beyond that the gap is structural. libzstd's inner loops use raw pointers with
no bounds checks and unaligned 64-bit loads throughout; ours compare-and-branch
on every slice access, several times per sequence. §6 puts
`forbid(unsafe_code)` on **parsing and table construction** and explicitly
allows audited unsafe in kernels, so moving the entropy inner loops behind the
backend seam is within the stated design rather than a breach of it — but it is
a decision to take deliberately, with the numbers in hand, not a tuning step.
**Parity is not reachable without it.**

> **Corrected by the ninth and tenth passes.** The last sentence was an
> inference, not a measurement, and the measurement contradicts it. Two of the
> three hot loops contain **no bounds checks at all**, and removing four of them
> from the third changed the number by nothing. Meanwhile a profile of libzstd
> itself puts the gap in one place: the fused sequence-decode-and-execute loop,
> 3.1x on both workloads. The route to parity that the evidence supports is
> **structural — fusing that loop, which needs no `unsafe`** — not unchecked
> indexing. The `forbid(unsafe_code)` claim stands, and nothing measured so far
> asks us to spend it.

The bit reader was the cheapest and the largest. `read(n)` was an `n`-iteration
loop with a bounds-checked byte load *per bit*; this reader's bit string is the
byte-reversed input read big-endian, which is exactly `u64::from_le_bytes` of
the eight bytes behind the cursor, so the whole thing collapses to one load, one
shift and one mask. `bits.rs` had reserved this for M3 in its own module doc.

#### One change that measured worse, and was reverted

Replacing the flat-path `resize(pos + len, 0)` — which zero-fills memory it
immediately overwrites — with `extend_from_within` doubling, so no byte is
written twice. The reasoning was sound and the result was 133 MB/s against 140.
The memset is a few bytes per match and the call overhead of the replacement
exceeded it. Reverted; the zero-fill stays.

#### The oracles are what made any of this safe

Every change here is index arithmetic whose failure mode is *plausible output*,
not a crash — the class §10 already records twice. So each fast path keeps the
obvious implementation it replaced, `#[cfg(test)]`, as its oracle:

- `BackwardBits::bit` → `backward_word_refill_matches_the_bit_at_a_time_reference`,
  every bit position and width from 1 to 32 across twelve buffer lengths,
  including past the end.
- `Window::byte_back`/`push_byte` → `wrapping_matches_the_byte_at_a_time_reference`,
  every combination of capacity, cursor phase, offset and length that fits.

This is the same discipline that keeps the scalar backend alive as the NEON
oracle, applied one layer down. Both tests were written before the measurement
was taken, and the reference corpus and `zstd-fixtures` error frames pass
unchanged.

#### The profiling harness is kept

`zalt-bench profile <file.zst> [seconds] [scalar]` decodes one frame in a loop
so a sampler can attach, and `just profile-decode <file.zst>` drives it with
macOS's own `sample` and prints the leaf-weighted table. Nothing needs
installing; `just profile` still drives Instruments when the question is finer
than "which function".

It is a recipe rather than a scratch file because of the specific thing it
caught. **A throughput table cannot distinguish "the kernels do not help" from
"the kernels never ran."** Both look like NEON ≈ scalar. Only attribution
separates them, and the answer was worth 50% on its own. The `scalar` argument
exists so both backends can be profiled from one binary: a hot function costing
the same in each is a function the acceleration is not reaching.

#### What is next, in profile order

**Stale — read the eighth pass instead.** This paragraph described a decoder
that no longer exists, and is kept only so the reasoning is not re-derived.

It said: `memmove` is 24%, being the output copy — every byte written to the
window, copied window → caller, then read again by XXH64, three passes where the
reference does one write and one read; closing it means letting the caller take
slices of the window instead of a `Vec` it owns, an API addition rather than a
tuning change.

Two of those three passes are already gone. The zero-copy `Sink` removed the
drain copy (measured at 0%, second pass), and `--no-check` removed the XXH64
pass on the frames the harness now builds — as it is absent on the wire traffic
this decoder targets, since the library defaults the checksum off. What the
eighth pass measures instead is 29.5% / 45.2% in match and literal copy, which
is the window write itself and already runs through the NEON kernels.

### M4 — Recipes and dictionaries
- [ ] Dictionary loading in L1 (`dict.rs`).
- [ ] `zalt-recipe`: manifest format, loading, content-type routing.
- [ ] `zalt-ztml`: content-prefix dictionary + manifest. No Rust logic if possible.
- [ ] `dcz` Compression Dictionary Transport.
- [ ] `zalt-recipe-train`: content-prefix v1, then trained dictionaries.

### M5 — Encoder
- [ ] Correctness-first encoder; `encode_decode` differential fuzzing.
- [ ] Parameter plumbing from recipes; stored-block fast path.
- [ ] Block splitting at statistical transitions (§7) — wire-compatible, no
      negotiation required.
- [ ] Ratio work only after correctness.

### M6 container header — design note

Recorded now because the reasoning is easy to lose, not because it is due.
**None of this touches the wire**, where frames are `ZSTD_FRAME` and carry
nothing of ours.

The temptation is to copy PNG's signature — `89 50 4E 47 0D 0A 1A 0A` — which is
the canonical example of magic-number engineering: `0x89` catches 7-bit
stripping, `PNG` identifies it to a human, `0D 0A` catches CRLF→LF, `1A` stops
DOS `type` spewing binary, `0A` catches LF→CRLF. Every byte earns its place.

**Every hazard on that list is extinct for a local cache file.** FTP ASCII mode,
mail gateways and DOS `type` are not in the path between our writer and our
reader on the same APFS volume. Copying those bytes would imitate the *form* of
the technique while missing its substance, which was: identify the hazards this
artifact actually faces, and spend a byte on each.

The hazards that are real here:

- **Version skew** — the browser auto-updates and leaves old cache files for a
  new binary. Far and away the most likely "corruption", and not corruption at
  all. Needs a major (refuse and discard) and a minor (ignore, forward-compatible).
- **Truncation** — power loss, crash mid-write, disk full, SIGKILL. A partial
  entry is likelier than any bit-level damage, and a magic at the front cannot
  detect it.
- **Silent bit rot** — APFS checksums metadata but deliberately **not user
  data**, on the reasoning that Apple's storage hardware carries its own ECC.
  Unlike ZFS or btrfs, a flipped bit in file contents comes back silently. This
  is a platform-specific argument for a content checksum.
- **Wrong file entirely** — a path bug feeds a `.png` to the cache reader. The
  magic already covers this.

Proposed layout:

```
off  size  field
  0     4  magic            5A 4C 54 FD
  4     1  format_major     unknown -> discard the entry, never parse
  5     1  format_minor     unknown -> ignore, forward-compatible
  6     2  flags
  8     8  payload_length   (LE)
 16     8  uncompressed_len (LE)
 24     8  content_xxh64
 32     4  header_xxh32     over bytes 0..32
 36    ..  payload
```

`payload_length` gives **truncation detection with no trailer**. PNG needs
`IEND` because a PNG is a stream that may be concatenated or embedded; a cache
entry is a discrete file we `stat` anyway, so `st_size != 36 + payload_length`
*is* the truncation check, costing nothing beyond a field we want regardless.

The two checksums are split because they answer different questions at different
costs: `header_xxh32` covers 32 bytes and says whether the file is worth opening;
`content_xxh64` requires reading all of it.

**A checksum is not authentication.** An attacker who can write the cache
directory controls decompressor input, and recomputes any checksum trivially.
These fields catch *accidents* — truncation, rot, skew. Defending the cache
against a local attacker needs a MAC keyed to something unreadable by them,
which is a browser threat-model decision with a real cost, not a container
format decision.

### M6 — `zalt::native` cache codec (gated)
- [ ] Design: interleaved rANS (8/16 lanes matching NEON width), NEON-friendly
      match encoding, incompressibility detector.
- [ ] Benchmark against `zstd -3` on cache-shaped workloads.
- [ ] **If it does not clearly win, do not ship it.** A second format is a real
      maintenance and security cost and only earns its place with numbers.

---

## 11. Open questions

1. **Does the browser need a zstd encoder at all?** If request-body compression
   is not planned, M5 shrinks to only what dictionary support and round-trip
   testing require — a large reduction.
2. **ZSS stylesheets.** You excluded them from the corpus, but the sample
   document references `zs="/style.zss"`, so they are fetched over the wire and
   will arrive zstd-encoded. Was the exclusion about not using *generic CSS* as a
   proxy, or is ZSS genuinely low-volume enough to ignore? They would be a
   natural fit for the same dictionary.
3. **Is WEFT ever fetched standalone?** If the data layer can be requested
   separately from the document, it is a distinct content type with very
   different statistics and deserves its own recipe entry.
4. **Threading model.** One decoder per response, or a shared pool? Determines
   whether instances must be `Send` and whether the budget is per-thread or global.
5. **Dictionary adoption path.** Given §7's caveat, is there a plan for origins
   to compress with a well-known ZTML dictionary ID — or should the effort go to
   `dcz` negotiation instead?
6. **`no_std` strictness.** Is the host willing to link `std`? If so, `no_std` in
   L1 is self-imposed discipline with a real cost; worth confirming it is wanted
   for the general-crate story.
7. **Recipe format.** TOML is assumed above. If recipes must be embedded in a
   `no_std` context, a binary format or `build.rs` codegen may serve better.

---

## 12. References

- RFC 8878 — Zstandard Compression and the `application/zstd` Media Type.
- Zstandard reference implementation (`facebook/zstd`) — `lib/decompress/`,
  and `decodecorpus` for test generation.
- Compression Dictionary Transport — defines the `dcb` / `dcz` content codings.
- Apple Silicon CPU Optimization Guide — Firestorm/Icestorm pipeline details.
- `core::arch::aarch64` — Rust NEON intrinsics.

---

## 13. Summary

The wire format is fixed; the freedom is in the layering. Build a clean,
general zstd codec that knows nothing about browsers or ZTML. Wrap it in an
embedding layer that knows about memory and hostile input but not about content.
Accelerate it with NEON behind a seam that keeps a scalar oracle alive. Then
express every content-specific tuning decision as *data* — a dictionary and a
manifest — so that ZTML is the first recipe rather than a feature.

If specializing for a new content class ever requires touching L1, the seam was
wrong, and widening it generically is the fix.
