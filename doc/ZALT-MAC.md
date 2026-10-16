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
    ├── zalt-archive/           # ZSTA archives: a .tar.zst that is also random-access
    ├── zsta-seekable-probe/    # test-only oracle: libzstd's seekable reader, zstd(1), tar
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

### Where the specification and the reference disagree

One case, found on the first day of M4, and recorded because a decoder cannot
split the difference.

RFC 8878 §4.2.1 says plainly: *"This specification limits the maximum code
length to 11 bits."* So a Huffman table derives `tableLog <= 11`, and zalt
enforced that everywhere.

`zstd-fixtures/golden-dictionaries/http-dict-missing-symbols` — the reference's
own regression fixture — carries a Huffman table whose weights sum to exactly
2048, which derives a `tableLog` of **12** and therefore 12-bit codes. zalt
refused it. The reference does not: `HUF_TABLELOG_MAX` is 12 in libzstd, and it
both compresses and decompresses through that dictionary happily.

The refusal was not a bug in the weight decoder, which is worth stating because
that was the first suspicion and it cost the most time to rule out. The decoded
weights satisfy every internal check the format offers — the remainder is
exactly 2048, a power of two, and the derived last weight is 12, inside
`WEIGHT_MAX`. Weights that were actually wrong would fail the rank-tiling check
almost certainly. The weights were right; the ceiling was wrong.

**zalt follows the reference, and only for dictionaries.** `huf::DICT_TABLELOG_MAX`
is 12; a literals section still may not declare more than 11. §1 makes this call
for us: the wire format is not ours to choose, and a browser that cannot read
what the ecosystem emits is broken. The ecosystem here is libzstd, which ships
this dictionary in its own test corpus.

It is worth being clear about what that costs. This is a deliberate divergence
from the specification text, in a codec whose first stated goal is to be a
clean, general implementation of that text. The narrowness is the mitigation —
one constant, one call site, and a literals section is unaffected — but it is a
divergence, not a reading, and a reader of §4 who checks against the RFC should
find it here rather than discover it in the source.

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

**And the encoder repeated it at M5.** The largest win there was `common_prefix`
a word at a time — the same shape as M3's word-at-a-time bit reader, in the
encoder instead of the decoder, found the same way and not predicted by anything
here either. Meanwhile the one change on this page's own logic — interleaving
two chain walks to expose independent dependency chains, which is the Firestorm
argument above applied exactly as written — measured nothing, for the reason
item #7 already gives about windows that fit in L2. The list keeps being right
about the machine and wrong about where the time is.

### Measurement discipline

Nothing lands without a benchmark. Build `zalt-bench` *before* the optimization
phase. Compare against system `zstd` 1.5.7 (`/opt/homebrew/bin/zstd`).

**Interleave the rivals; quote the ratio.** Two implementations timed one after
the other are timed in different windows of wall-clock time, and on this machine
that is the difference between a result and a rumour — §10 records the same
encode measured at 0.37 s and then 0.17 s back to back. Alternating their
batches does not quiet the machine, it makes the interference *shared*, so a
thermal step or a core migration moves both columns and leaves the ratio where
it was. An absolute MB/s off this laptop is a number about the laptop; the gap
ratio is the part that transfers. `zalt-bench encode` works this way and links
libzstd in rather than shelling out to it, which is what makes a 3 KB file
measure an encoder rather than a `posix_spawn`.

**A change that is supposed to leave the output alone must be shown to.** Every
encoder change in M5's throughput pass was checked byte-for-byte against the
ratio table before it was timed, which is also what makes an A/B between two
builds worth anything: binaries that emit identical bytes differ in exactly one
thing, so the difference between them is not a second encoder being measured.

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

> **Measured at M4: the premise needs checking before this is built.** The
> paragraph assumes markup and hash regions can be separated into different
> blocks. In a real `.zdoc` the hashes are interleaved at roughly 64-byte
> granularity — every subresource reference carries one — rather than
> segregated. Splitting at a 1 KB stride buys 2.4–5.1% of literal entropy while
> each extra Huffman table costs 100–200 bytes, so it is a net loss at that
> granularity. See M4.

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

> **Measured at M4, and the last sentence needs qualifying.** The content prefix
> is worth 22.5–35.5% held out, and `zstd --train` over the same samples is
> *worse* on five of six documents — COVER has too few samples to select from at
> this corpus size. The upgrade to a formatted dictionary is therefore about
> `Dictionary_ID` and pre-built entropy tables, not ratio. See M4.

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

### Generated valid frames — `decodecorpus`

`just decodecorpus [count] [seed]`.

The fuzz targets have a blind spot that is easy to miss: `roundtrip_valid`
compresses the fuzzer's bytes with **libzstd's encoder**, so it can only ever
reach encodings libzstd *chooses to emit*. An optimising encoder never produces
most of the legal format. This section already noticed the symptom without
naming the cause — `rle-first-block` is described as "a block type encoders
almost never emit", and exactly one example is kept by hand.

`decodecorpus` is facebook/zstd's generator of random **valid** frames, and it
walks the legal space deliberately: every literals mode, every block type,
unusual window sizes, offset-code paths no encoder picks. Each frame comes with
its expected output, so a disagreement is unambiguous rather than a judgement
call.

**The vendored copy never compiled.** `zstd-fixtures/decodecorpus.c` has been in
the tree unreferenced, and it cannot build there: it includes zstd's internal
headers and `compress/zstd_compress.c` outright, so it only builds inside the
zstd source tree. The recipe fetches that tree at the version the reference
binary reports — so the generator always matches the oracle — and builds it in
`target/`.

**Result: 4,500 frames across three seeds decode byte-exactly.** No
disagreement with the reference.

#### The first version of this recipe tested nothing

Worth recording, because it is the exact failure this section exists to guard
against and it was self-inflicted. `decodecorpus` writes expected output only
when given `-o`; without it there are frames and nothing to compare them to.
The comparison was written as

```sh
if [ -f "$exp" ] && ! cmp -s "$f.zalt" "$exp"; then ...
```

so when `$exp` did not exist the check was skipped **silently**, and the recipe
reported "all 2500 frames decode and match the reference" having compared none
of them. It had verified only that decoding returned success — which for a
decoder is the weakest property there is, and precisely the one §9 warns
round-trip testing already over-supplies.

A missing expected file is now a failure, and the recipe prints how many frames
it actually compared rather than how many it generated. **A guard that skips a
check on absent data turns a test into a formality**, and it reads as
defensiveness while doing the opposite.

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

### Four checks that ask different questions

Added together, and worth separating because each answers something the others
cannot.

| tool | question | answer here |
|---|---|---|
| `just coverage` | was this line *run*? | wired; needs `cargo-llvm-cov` |
| `just mutants` | if this line were *wrong*, would anything fail? | 49/50 caught on `xxh64.rs` |
| `tests/properties.rs` | do the stated invariants hold on generated input? | 7 properties, in `just t` |
| `tests/third_oracle.rs` | do *two independent readings of the RFC* agree? | 403 frames, no disagreement |

**Coverage** exists because §10 records the failure it would have caught: the
entire fixture set used `FSE_Compressed` mode, so the predefined FSE tables were
never exercised by any test, and the `ML_DEFAULT` bug lived in that gap. A dark
region in a report is the cheap version of "what should we test next".

**Mutation testing** goes further, and is the better fit for this codebase.
Coverage would have shown the predefined-table path as *covered* once any test
touched it; mutation testing asks whether changing it makes a test fail. The
`ML_DEFAULT` bug was a wrong constant that still summed to 64 — precisely a
mutant that survives. Its first run found 1 missed mutant of 50 in `xxh64.rs`:
`buf_len > 0` → `buf_len >= 0`. That one turns out to be **equivalent** —
entering the partial-stripe branch with an empty buffer either returns early
having stored the same bytes, or consumes the same stripe, so no output differs
and no test can catch it. What the guard actually protects is the extra copy
this module's own comment says was removed to halve the checksum's memory
traffic: a *performance* property, which mutation testing cannot see. Equivalent
mutants are the technique's known false-positive mode and this is a textbook
one.

**Property tests** overlap the fuzz targets deliberately. The difference is
cadence and visibility: `cargo fuzz` runs for hours on demand, this runs in
milliseconds on every `just t`, and the properties are *written down in the
repository* rather than living in a campaign someone has to remember to start.
The one worth naming checks the argument `DecodeTable::entry_masked` relies on
to skip its bounds check — that a table which passed validation can only produce
in-range states. That argument was a comment; now it is checked.

**The third oracle** closes a gap nothing else could. Every other check here
ultimately asks libzstd, so a *shared misreading* of RFC 8878 would be invisible.
`ruzstd` is a pure-Rust decoder sharing no code with either; the only thing all
three have in common is the specification, so agreement is evidence about the
spec rather than about the reference.

Its asymmetry differs from §9's and is weaker. "zalt accepts what libzstd
rejects" is always a bug; that does not transfer, because `ruzstd` is neither
the reference nor a subset of it. The only sound assertion is conditional — **if
`ruzstd` decodes a frame, zalt must decode it to the same bytes** — and a frame
it declines says nothing either way.

It also had to be pointed at real material. Against the golden corpus alone it
compared **three frames**, which is ceremony rather than testing; wired to the
`decodecorpus` output it compares 400, and prints the count so that a version
comparing nothing cannot pass quietly.

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

**`compress` came with M5's levels.** `zalt c -19 file -o file.zst`, zstd's own
spelling of a level because muscle memory is a real interface, as the parser
argues for `-d`. Three choices differ from the library on purpose. **A content
checksum is written by default**, as `zstd(1)` writes one: a file at rest is
where corruption goes unnoticed, while the library's default serves a wire that
has its own integrity. **A level the build does not implement is refused**, not
clamped as the library clamps it: someone typing `-22` expects libzstd's 22,
with its 128 MiB window, and would otherwise get level 19 with no sign of the
substitution. **Compressed bytes are not written to a terminal**, as `zstd(1)`
refuses too. `--max-window-log` means the same ceiling in both directions, and
a decode-only flag given to `compress` (or the reverse) is an error rather than
silently ignored. Each input is read whole — the encoder is one-shot — and
several inputs to stdout become frames back to back, which every conformant
decoder reads as their concatenation.

**Known gap.** The C ABI exposes `zalt_decoder_footprint` but no `Budget`,
`DecoderPool` or `BufferSource` — a C host can *observe* what a decoder holds
and account for it, but cannot hand zalt a ceiling or an arena. That matches the
boundary's stated philosophy for threading ("the host already has a model and
does not need ours"), and it is defensible for footprint too. It is recorded
here rather than assumed, because a Mac app embedding this in-process is exactly
the host the allocator seam was built for, and it currently cannot reach it.

#### The threading model, decided — and the pool measured under it

§11's open question four asked *"one decoder per response, or a shared pool?"*
on GRAZER's behalf. Since zalt and GRAZER have one author, it is now decided:
**multithreaded, both shapes supported.** zalt still starts no threads — the
host owns the threading model — but every type a host would move or share is
asserted `Send`, and the pool and budget `Sync`, in
`crates/zalt-embed/tests/thread_safety.rs`. All of it held *incidentally*
before, with no `unsafe impl` anywhere, which is another word for undefended.

**A single frame cannot be decoded in parallel, and that is the format's
decision, not ours.** Matches reach back across block boundaries, so block N
needs block N−1; libzstd has no multithreaded decoder for one frame either. The
parallelism available is *across* resources, which is exactly §8's push model.

`zalt-bench concurrent` measures it, running the same work two ways — through
the shared pool, and with one decoder per thread sharing nothing — so the gap
between them at a given thread count is the cost of sharing and nothing else.
Every thread count and both shapes alternate inside one loop.

**It needs a quiet machine in a way no other measurement here does.**
Interleaving rescues a comparison between two implementations, because a
disturbance lands on both and cancels. It cannot rescue a comparison between
thread counts: with no idle cores, an added thread has nowhere to run and the
row measures the scheduler. The first attempts produced 113% and 119%
"efficiency" — arithmetically impossible — with the swap file 94% full, so the
harness now **voids its own run** when any row is superlinear, rather than
printing an impossible number beside plausible ones. §9's rule about a guard
that skips a check on absent data applies to benchmarks too.

On a quiet machine (fresh boot, language servers suspended), the small `.zdoc`
documents:

| idle set | pooled, 1 thread | pooled, 8 threads | against one decoder per thread |
|---|---:|---:|---:|
| one `Mutex<Vec<Decoder>>` | ~158 MB/s | ~946 MB/s | 0.84x at 8 threads |
| sharded per thread | ~159 MB/s | ~1 038 MB/s | 0.92x |
| sharded, decoders boxed | **~170 MB/s** | **~1 123 MB/s** | **1.00x** |

**+19% at eight threads, and the pool now costs nothing.** What made the
measurement useful was that the pool trailed the control by ~7% **at one
thread**, where nothing contends. That split the cost into two parts of
different kinds:

- **Contention**, which grew with thread count. Each thread now has a shard it
  reaches for first; a miss falls through to the others with `try_lock` and
  never blocks. `max_idle` stays one exact global cap in an atomic, because a
  per-shard cap would be a memory ceiling that moves with how many threads
  touched the pool. A test returns 32 decoders from eight threads at once and
  requires exactly `max_idle` parked and exactly that many reservations
  charged; it was checked against a mutant that drops the cap (32 parked).
- **Copying**, which was flat. Profiled side by side, the pooled and unpooled
  runs differed in one line: 14% more `_platform_memmove`. A `Decoder` carries
  its entropy tables inline and is **13 944 bytes**, and a checkout moved it by
  value four to six times — on few-kilobyte resources, more bytes copied than
  decoded. Boxing makes each move a pointer, for one allocation per decoder
  created rather than per resource served.

**The first guess about the one-thread cost was wrong**, and worth recording
because it was plausible. The pool shrinks a returning decoder above 256 KiB,
and the corpus holds one 219 KB document, so the suspicion was a reallocation
on every reuse. Removing that document made the cost *worse* (0.92x, and 0.82x
at eight threads) — the overhead was per checkout, so it weighed more on
smaller resources. One control falsified it; the profile then found the copy.

**Two smaller results.** The control itself scales to 3.9x on four threads
(98%) and 6.7x on eight (84%) on this 4P+4E machine. An earlier reading of
5.42x at eight threads looked like an exact match for the efficiency-core
split, and was not: it came from the noisy machine, and the quiet one gives
6.68x. And a first attempt at sharding was **reverted** for measuring nothing —
on the 94%-full swap file. On the quiet machine the same change reproduces to
about 1%. Neither result was wrong as a statement about its measurement; the
measurement was wrong about the machine.

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
- [~] Double-symbol Huffman tables — implemented and reverted twice. The
      sixteenth pass re-ran it under the current regime: **worse in all twelve
      rounds**, but by 1.7% rather than 8.4%. Conflicts with the four-stream
      interleave, as predicted.
- [x] Per-block FSE table clones removed (+9.6% on real `.zdoc`).
- [x] Checksum skipped when the frame declares none (8% on a quiet machine; the
      ~26% first recorded was memory pressure — see the seventh pass).
- [~] Match-source prefetch — tried, 3.5% slower, struck. (NEON XXH64 struck
      too — see §5.)
- [x] `backend_equivalence` fuzzing clean — 26,916 runs under ASan.
- [~] `zalt-darwin` — profiling does not justify it; M2's pool captures the
      memory cost portably. Not built. **Re-checked in the sixteenth pass
      against the current decoder: still zero.**

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

**The rest of the sweep, and where it ran out.** With the mechanism established,
every non-cold call in the hot path was inventoried from the disassembly and
sorted by how often it runs. Three were per-sequence — `sequences::resolve_offset`
and two kernel calls out of the executor — and were closed, the kernels by
hoisting the short non-overlapping match into `Neon::copy_match` so a
handful-of-bytes copy never calls out at all. The rest are per *block*
(`resolve_table`, `fse::State::new`, `BackwardBits::new`, everything in
`literals::decode`) and were left, because a call amortised over a whole block
is not worth inlining a large function for.

That sweep is worth **1–3%**, and it is the first result this section has had to
report with two estimators disagreeing. By round count it is clear: the changed
binary won 12 of 14 rounds on `.zdoc` and 5 of 5 on the code corpus, and the
gap ratio improved in almost every one (`.zdoc` 2.75x → 2.65x, corpus 2.63x →
2.58x). By *best-of* — this document's usual estimator — it is roughly neutral,
because the unchanged binary produced occasional outliers as fast as the changed
one while typically running 3% slower.

Best-of assumes interference is one-sided, and a bimodal run like that is a
signal the assumption has stopped holding — the outliers look like code layout
rather than the machine. Kept, on the round count and the ratio, and recorded
here as a smaller and less certain result than the two before it.

**`#[inline]` is a hint, and on the hottest function in the decoder it was
ignored.** Three of this milestone's four wins are now the same mechanism —
removing a call boundary the compiler chose to keep — and in every case the
disassembly said so plainly while the source looked fine. The profile could not
have found this: `sample` attributes leaf time to `word_at`, which is a real cost
correctly reported, and says nothing about the spill around it. Counting calls
in the disassembly and comparing against the reference is what found it, twice.

#### Fifteenth pass: the memset that keeps not being where it is looked for

Recorded as an open item rather than a result. **`_platform_memset` has been
6% of the `.zdoc` profile all session, three changes have been aimed at it, and
it has not moved.**

| aimed at | outcome |
|---|---|
| `DecodeTable`'s 512-entry array (twelfth pass) | build 24% cheaper, `memset` 446 → 438, end-to-end **0%** |
| `huf::Table`'s per-block `Vec` (thirteenth pass) | **+4.6% / +7.0%** — real, but `memset` share still rose |
| `DecodeTable`'s build scratch (this pass) | ~+2% unconfirmed, `memset` 6.1% → **6.4%** |

Only the middle one paid, and even that did not reduce the symbol it was
chosen to reduce — it removed a `malloc` per *block*, which the allocator
samples showed and the `memset` attribution did not.

The scratch hoist is **kept and marked unproven**: it does strictly less work,
and the +2% was ten of twelve clean rounds — but on a machine whose canary had
fallen from ~1420 to 1230, and with a profile that declines to confirm it. It
should be re-measured on a quiet machine before anyone believes the figure, and
it should not be cited as evidence that the scratch was the cost.

**The likeliest remaining source is `Window::copy_match`'s growth path**, which
`resize`s with zeroes over bytes it overwrites immediately afterwards — once per
match, and a frame smaller than its window is *entirely* growth path, which is
every real `.zdoc`. §10 already records one failed attempt to remove it
(`extend_from_within`, 133 against 140 MB/s), so it is known ground and known
hard, and it is the one candidate left that is per-match rather than per-block.

The lesson is narrow and worth stating plainly: **a profiler names the function
that runs, not the code that caused it to run.** `memset` is called from
everywhere, and three of four guesses about which caller were wrong. Attribution
by inference has now cost more than it has returned; the next attempt should
find the call site rather than reason about which one it must be.

#### Sixteenth pass: re-testing the reverted changes, and the first reversal

The verdicts in this section were taken on a machine now known to swing 55%
with no code involved, and against raw MB/s rather than a ratio. Each is being
re-run under the current regime: two binaries alternating, **zstd measured in
the same window as the oracle**, `ruzstd` alongside for context, and the canary
bracketing the run.

**`extend_from_within` instead of the window zero-fill — reversed.**

| corpus | gap before | gap after | rounds won |
|---|---:|---:|---|
| real `.zdoc` | 2.61x | **2.46x** | 6 of 6 |
| 61 MB corpus | 2.60x | **2.53x** | 4 of 4 |

Recorded here as **5% slower** (133 against 140 MB/s). It now closes the gap by
about 6% on the target workload, with the distributions not overlapping and the
canary drifting 0.6%.

The original number was not wrong when it was taken; it **expired**. The
zero-fill was a small share of a slow decoder, and the eleventh and fourteenth
passes made the decoder around it much faster — inlining the window operations
into the executor and the bit reader into its callers. A cost that was 5% of the
work becomes a different proposition when the rest of the work halves. *Every
reverted change in this section was measured against a decoder that no longer
exists.*

**It also closes the open question from the fifteenth pass.** `_platform_memset`
fell from **547 samples to 62**. That is the ~6% which survived three attempts
aimed at the FSE entry array, the Huffman `Vec`, and the FSE build scratch —
none of which was the cause. It was `Window::copy_match`'s `resize(pos + take,
0)`, zero-filling bytes it overwrites on the next line, once per match; and
because a frame smaller than its window never leaves that path, on a real
`.zdoc` that is *every match in the document*. The fifteenth pass named this
candidate and did not test it, having spent its credibility on three guesses
already.

**`zalt-darwin` re-checked, and the answer has not moved.**

The crate was struck on the grounds that no OS or allocator symbol appeared in
either profile. The decoder has roughly doubled in speed since, which raises
every fixed cost's share, so the claim was worth re-testing rather than
inheriting.

| | real `.zdoc` | 61 MB corpus |
|---|---:|---:|
| allocator | 2.66% | 0.00% |
| virtual memory (`mach_vm`, `madvise`, `mmap`) | **0.00%** | **0.00%** |
| dispatch / threads | **0.00%** | **0.00%** |

All three of the crate's premises are absent. The only OS cost is 2.66% of the
allocator on small documents, and that is the case M2's `Recycler` and
`DecoderPool` already answer *portably* — which §10 argued is a better outcome
than a `mach_vm` dependency, and remains one.

One caveat stated rather than buried: this profiles the `sink` path, a reused
decoder that allocates nothing per frame, because that is the deployed shape.
§8's push model — many small resources decoded concurrently — would exercise
allocation harder than any of this shows. That is an argument for measuring the
pool under concurrency, not for a Darwin VM layer.

> **Measured since.** The pool under concurrency cost 16% at eight threads on
> small documents, from lock contention and from copying a 14 KB decoder by
> value — both portable fixes, and neither an OS cost. See M2's threading notes.
> The `zalt-darwin` verdict stands.

*(A false start worth recording: the first attempt attributed 35% to the OS,
because the symbol filter matched "kernel" and `zalt::kernels` is the
accelerated copy module. The figure was nonsense and looked entirely
plausible.)*

**Double-symbol Huffman, re-implemented and re-measured.**

The paired table is derived from the single-symbol one exactly as the sixth
pass describes: for a `table_log`-bit window, the second symbol's bits are all
present precisely when `a.nb_bits + b.nb_bits <= table_log`, which is both the
pairing rule and its validity test. `decode_four` grew four independent cursors,
because streams emitting one or two symbols per step no longer share an index —
the conflict with the four-stream interleave that §10 predicted would cost more
than pairing returns.

One subtlety the earlier note does not record: a pair whose second symbol has no
room left in the segment **cannot be split**, because the entry stores the
*combined* bit count and skipping it would consume bits belonging to a symbol
never written. The paired loop therefore runs only while two symbols fit, with a
single-symbol tail — which is another branch per stream per step.

Correctness first: 304 tests, and 600 `decodecorpus` frames decoded byte-exactly
against the reference. It works; it is simply slower.

| corpus | gap before | gap after | rounds |
|---|---:|---:|---|
| real `.zdoc` | 2.48x | **2.52x** | worse in 8 of 8 |
| 61 MB corpus | 2.57x | **2.61x** | worse in 4 of 4 |

Worse in **all twelve rounds across both corpora**, with the canary drifting
**0.0%** — the steadiest measurement in this section, taken on a machine that
was thermally capped at about 61% of its healthy speed. A cap is not a problem
for a *ratio*: it slows both sides, and what a ratio needs is that conditions
hold still, which a cap does better than a busy machine ever does.

So the verdict stands and the magnitude does not: **8.4% became 1.7%**. That is
the same shape as the flattened code tables, and for the same reason — the
decoder around the change got much faster, so a fixed cost that used to be a
large share of a slow loop is now a small share of a quick one. The direction
survived; the confidence in it should not be inherited from the old number.

**The rest of the re-runs so far.**

| change | recorded verdict | re-tested verdict |
|---|---|---|
| `extend_from_within` for the window zero-fill | 5% slower | **reversed** — gap 2.61x → 2.46x |
| Flatten LL/ML code tables | 9% slower | **confirmed slower**, but only ~1% (gap 2.46x → 2.49x) |
| Match-source prefetch | 3.5% slower | **confirmed no gain** — gap ~2.50x vs ~2.53x, 3 rounds each way |
| Fuse sequence decode with execution | ~4% slower | **confirmed**, and worse: 6.4% (twelfth pass) |
| Hoist the xxh64 accumulators | no change | **not measurable** — see below |
| Double-symbol Huffman tables | 8.4% slower | **confirmed slower**, but ~1.7% (gap 2.48x → 2.52x) |
| `zalt-darwin` | not justified | **confirmed** — 0.00% VM, 0.00% dispatch |

Two of these are worth a note beyond the verdict.

**The flattened code tables lost by 1%, not 9%.** The direction held, so the
original decision stands and the range `match` stays. But an 9x shrink in the
margin is itself information: the low codes resolving in a compare with no
memory access mattered far more when the loop around them was slower. A verdict
recorded as decisive is now marginal, and would flip if the surrounding code
changed again.

**The prefetch re-test agrees with the reference's own profile.** §5 struck it
on the argument that a 2 MiB window sits inside the M1's 12 MiB shared L2, so
the load is already fast and the hint only adds instructions. The tenth pass
then found `ZSTD_decompressSequencesLong` — libzstd's *prefetching* variant — at
25 samples of 9182. Three independent lines now agree, which is as settled as
anything in this section gets. Note the implementation had to reach `prfm`
through `asm!`: `core::arch::aarch64::_prefetch` is still unstable, and §2 pins
stable deliberately.

**The xxh64 accumulator hoist cannot be re-tested on this corpus**, and saying
so is the honest result. The corpus is built with `--no-check` since the twelfth
pass, because that is what the library emits and therefore what the wire
carries — so the checksum never runs and the measurement would be of nothing at
all. Re-testing it needs a checksummed corpus built for the purpose, and the
original figure was "no change" measured against a decoder where the optimiser
was already promoting the accumulators out of `&mut self`.

For context, the third column is now measured in the same runs: zalt against
`ruzstd`, the independent Rust decoder, is **1.26x faster on `.zdoc` and 1.4x on
the code corpus** — while sitting 2.5x from the C reference. That is the number
that says how much of the remaining gap is "safe Rust" and how much is ours.

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
- [x] Dictionary loading in L1 (`dict.rs`) — formatted and raw-content, with
      the reference's own `golden-dictionaries` corpus as the conformance test.
- [x] Wired into the decoder: `Decoder::load_dictionary`, content seeded as
      reachable history, entropy tables and repeat offsets seeded per frame,
      `Dictionary_ID` matched. 12 reference-compressed frames decode
      byte-exactly.
- [x] `zalt-recipe`: manifest format, loading, content-type routing. `no_std`,
      parser optional, one dependency (`sha2`, see `dcz`). 31 tests, of which
      the rejection table is the half that matters.
- [x] `zalt-ztml`: content-prefix dictionary + manifest. 63 lines of Rust, both
      of them data; 47 KB dictionary; held-out output 23–36% smaller at L9,
      a 29.0% mean.
- [x] `dcz` Compression Dictionary Transport (RFC 9842, saved as
      `doc/rfc9842.txt`): header parse, `sha2`, hash-indexed dictionary store,
      two structured fuzz targets. Verified against the reference in both
      directions.
- [x] `zalt-recipe-train`: content-prefix v1. `build` / `eval` / `sweep`,
      leave-one-out throughout. Replaces `ci/gen-ztml-dict.py` and reproduces
      its 47 KB blob byte for byte. Trained dictionaries deliberately not
      built — see below.

#### Wiring the dictionary in, and the two bounds it moved

Loading a dictionary is a parser. *Using* one changes what "how far back may a
match reach" means, in two places that had been constants.

**The window keeps the dictionary separate from its own history.** Prepending
the content into the ring would be simpler and wrong in both directions: a
dictionary larger than the window would be truncated, losing reach the format
allows, and one smaller would stay addressable long after the format says it is
gone. So `Window` carries a `dict` alongside `buf`, and a match whose offset
runs past this frame's output starts that many bytes from the end of the
dictionary — a plain append, since the two buffers cannot overlap — with
whatever the match still needs continuing from `output[0]` as an ordinary
in-window match.

**Reach is time-limited, and the rule is easy to get one byte wrong.** RFC 8878
§5 makes the dictionary addressable "as long as the amount of data decoded from
this frame is less than or equal to `Window_Size`". The first implementation
tied that to the ring being full, which is true one byte *earlier*: `is_full()`
holds at exactly a window's worth of output, where the format still permits the
reference. Now the condition is `total > capacity`, which is the sentence.

**The bug the end-to-end test actually found was elsewhere.** The executor
carried `if seq.offset > frame.window_size { reject }`, written when a frame
could not have a dictionary and correct until it could. §5 says a sequence may
name "parts of the dictionary with offsets larger than `Window_Size`", so that
bound refuses legal frames — and did, on the very first reference-compressed
frame tried, at offset 1320 against a 1189-byte window. The bound is now
`window_size + dict_reach()`.

Worth recording *how* that was found, because the first two guesses were both
wrong. The error text named an offset and an available size, and both the
dictionary loader and the window's own check were suspected before a debug print
showed the window check never firing at all — the message was coming from a
third place that looked nothing like the others. **A shared error variant makes
two different rejections indistinguishable in their message**, and the minutes
lost were spent proving which one had spoken.

The tests that hold this up are `frames_compressed_with_a_dictionary_decode_byte_exactly`,
against 12 frames the reference compressed with a dictionary at three levels,
compared byte for byte — plus a `checked > 0` assertion, because a loop over
fixtures nobody generated passes exactly as quietly as one that verified them.

#### The recipe crate, and the direction a limit is allowed to move

`zalt-recipe` is the mechanism from §7 with no instance in it. Four decisions
the sketch left open, and one of them is load-bearing.

**A recipe may narrow a host's decode policy. It may never widen it.** Every
ceiling folds by minimum: `Decode::narrow` against an 8 MiB host cap returns
8 MiB whatever the manifest asked for. This is the reason the crate exists in
the shape it does. A manifest is configuration, and configuration travels — it
arrives from a scanned directory, from a path on a command line, from whatever
shipped beside a dictionary. If a recipe could *raise* a window ceiling, every
one of those paths would become a way to talk a host out of the bound it set
for untrusted input, which is §6's amplification primitive re-introduced one
layer up where the decoder that checks it cannot see. Refusing the load instead
would be no better: it would let a recipe deny service to a host whose policy is
merely stricter than the recipe expected. So it narrows silently, and two tests
pin both directions.

**No dependencies, so the manifest parser is hand-written.** `serde` + `toml`
would have been the largest dependency event in the project, for a six-table
config file, in a crate on the layering diagram's untrusted side. `zalt-cli`
had already taken this position for the command line. The parser implements
exactly the subset §7's sketch uses and **refuses everything else by name** —
floats, datetimes, dotted keys, inline tables, literal and multi-line strings,
nested arrays, unknown tables, unknown keys, duplicate keys. That last group is
the point. A partial parser that quietly mis-reads syntax it does not support
produces a manifest that loads cleanly and tunes nothing, and *a tuning
parameter that silently fails to apply is invisible*, because the only symptom
is a ratio nobody measured against the alternative. `[encoder]` when the table
is `[encode]` now fails at load with a line number instead of at a benchmark
three weeks later.

**The manifest's `Dictionary_ID` is checked, not trusted.** §7's caveat is that
a frame can only be decoded with the dictionary that compressed it, so a
manifest whose declared id disagrees with the dictionary on disk does not
produce an error later — it produces wrong bytes. One comparison at load time
closes it.

**Two recipes claiming one content type is an error, not a precedence rule.**
Declaration order, filename, specificity are all defensible tie-breaks and none
is guessable from reading the manifests; a directory load would resolve to
whichever file the filesystem enumerated first, which is an outcome rather than
a rule. `RecipeSet::insert` refuses the second claim, and `load_directory` sorts
before loading so the failure is identical on every machine. Within one recipe,
`[[rule]]` order *is* the tie-break — those were written together by one author
who can see all of them at once, and two manifests in a directory were not.

**The layering grep caught its own author inside the hour.** A unit test in
`recipe.rs` used `.zdoc` as a sample extension — in the crate whose entire
purpose is to be the content-blind mechanism. The fix was a different string,
per the script's own instruction that the answer is essentially never to add the
word to the allowlist. Worth recording is what the failure clarified: the check
greps `src/` and not `tests/` **on purpose**, because the rule is about
behaviour that branches on a content class, and `zalt-codec`'s `text.bin` has
been WEFT-shaped corpus data since M1 on the same reasoning. That scoping was
implicit until something made it load-bearing; it is now written down in the
script.

The crate builds three ways, and `just no-std` now checks all of them:
`default`, `--no-default-features --features parse`, and bare
`--no-default-features` — the last being the browser's path, a `Recipe` baked in
as a Rust value with no parser in the binary at all. That third build is what
keeps the split honest, since a `String` drifting into the data types would
still compile with `parse` on.

#### The first recipe, and four things the corpus said

`zalt-ztml` is a manifest, a 47 KB dictionary, and 63 lines of Rust — which is
the seam in `zalt-recipe` working rather than a crate left unfinished.
`ci/layering.sh` now counts those lines and fails above 200, because §3 states
the property in prose and prose does not fail a build.

**What it is worth, held out.** Leave-one-out — rebuild the dictionary without
the document being measured, because measuring against a dictionary containing
the file measures memorisation:

| document | bytes | L6 plain | L6 + dict | smaller by |
|---|---|---|---|---|
| `showcase.zdoc` | 7 782 | 2 919 | 1 880 | **35.5%** |
| `scripts.zdoc` | 3 441 | 1 799 | 1 267 | 29.5% |
| `painted.zdoc` | 3 492 | 1 691 | 1 216 | 28.0% |
| `handsome.zdoc` | 11 755 | 4 019 | 3 057 | 23.9% |
| `antikythera.zdoc` | 9 083 | 3 258 | 2 485 | 23.7% |
| `christmas.zdoc` | 8 890 | 3 543 | 2 744 | 22.5% |

`just ztml-ratio` reproduces it. §7's "it works surprisingly well" is now a
number.

**Trained dictionaries are not currently an upgrade, and that reverses §7's
staging.** `zstd --train` over the same held-in samples loses to plain
concatenation on five of six documents (3.52 vs 3.69, 2.96 vs 3.16, 3.58 vs
3.88, 2.67 vs 2.82, 3.69 vs 4.11; `painted` ties). COVER needs many samples to
select from, and six documents totalling 37 KB is not many — the trainer
discards material the raw prefix keeps. So the reason to move to a formatted
dictionary is **not ratio**. It is that a formatted dictionary carries a
`Dictionary_ID` and pre-built entropy tables, and a raw content prefix carries
neither. A frame compressed against this dictionary declares ID 0, so a decoder
handed the wrong content prefix cannot tell, and produces wrong bytes caught
only by a checksum that may be absent.

> **Corrected while implementing `dcz`, the next item on this list.** The
> paragraph above ended "safe where both ends are versioned together; not safe
> as a negotiated wire dictionary". That is wrong, and reading RFC 9842 instead
> of summarising it is what showed why. §5: *"The dictionary used for the 'dcz'
> content encoding is a 'raw' dictionary type."* `dcz` carries the dictionary's
> SHA-256 in a header ahead of the frame, so on that path the identity arrives
> out of band and is **stronger** than the 32-bit `Dictionary_ID` it replaces.
> A content-prefix dictionary is not a second-class thing to be upgraded away
> from for the wire; it is the shape the transport specifies. What survives is
> narrower: the missing `Dictionary_ID` matters only where a dictionary is
> negotiated *without* `dcz`, and pre-built entropy tables remain a real
> per-frame win on small resources that a content prefix cannot provide.

**Order is part of the artifact.** The same bytes concatenated in a different
order measured up to 45 bytes apart on a 1 218-byte output — 3.7%. zstd matches
backwards from the dictionary's end, so material nearest the end encodes with
the cheapest offsets. A generator that concatenates in filesystem order
therefore produces a different dictionary on a different machine, silently, and
the only symptom is a ratio nobody compared. Hence `ci/gen-ztml-dict.py` rather
than a shell loop, an explicit sort, a provenance file recording the order and
per-input hashes, and `just ztml-dict-check` in the gate.

**A dictionary is not free on large inputs.** `synthetic.zdoc` is 219 KB and is
deliberately *excluded* from the dictionary: including it makes the held-out
ratio slightly worse while growing the blob sevenfold. Compressing it against
the shipped dictionary is worth −3.1% at L3 and **+6.1% at L6** — actively
worse. A large file has enough of its own history that the dictionary adds
nothing and can misfit the entropy tables. This is §8's small-resource claim
confirmed from the other side, and it is an encoder-side decision: a recipe that
applies a dictionary above some size is doing harm. Worth a size threshold in
the manifest when M5 needs one.

#### Should the dictionary carry the whole vocabulary? Yes, but not for ratio

§8 says ZTML's closed vocabulary makes it an unusually good dictionary target.
Measured, the vocabulary block earns almost nothing on ratio once real samples
are present — five of six documents are unchanged to two decimal places, because
the samples already contain every tag, in context, and context matches better
than a bare fragment.

The exception is the whole story. `christmas.zdoc` uses all 88 elements, and
held out, **43 of them appear in no other sample** — and it is the one document
the vocabulary block improves (3.16 → 3.25). The block is not a ratio win, it is
**tail insurance**: it pays exactly when a document uses vocabulary the corpus
lacks, which is the deployment case, not the test case. At 2.5 KB of a 47 KB
dictionary it is cheap, and it is kept.

The measurement **understates** it, unavoidably. The list was extracted from
`doc/*.zdoc`, so by construction it cannot contain an element the samples never
use — precisely the case it exists to cover. `data/vocabulary.txt` is therefore
a plain, swappable list, to be replaced by the ZTML specification's own element
and attribute set when that lands. The generator does not care where the lines
come from.

#### `dcz`, and the value of reading the RFC rather than the summary

The header is forty bytes and §7 described it correctly, so this looked like an
afternoon. Fetching RFC 9842 changed two decisions and corrected one written
claim, which is the argument for keeping the text next to `rfc8878.txt` rather
than working from a paraphrase.

**The correction is above**, in the M4 dictionary notes: `dcz` dictionaries are
*raw* by specification, and the SHA-256 in the header is the identity. The
content-prefix dictionary `zalt-ztml` ships is therefore not a compromise on
this path — it is the specified shape.

**It lives in L3, not L1.** Layering permits either; `dcz` is a transport
format, not a content class, and `magic.rs` already carries the constant. The
deciding argument is cohesion: the whole purpose of the header is *which
dictionary to use*, which is row one of §7's table. `zalt-codec` stays RFC 8878.

**SHA-256 is not a choice.** §5 fixes the 32 bytes as a SHA-256 of the
dictionary, and the origin computes them; a faster hash would simply never
match. It is also not a cost — it runs once per dictionary *load*, never per
frame.

**And it is `sha2`, after being hand-written first and found wrong.** The
argument for writing it was the workspace's zero-dependency position, plus the
observation that SHA-256 has no key, no parameters and published vectors — the
one primitive where rolling it looks defensible. The bug below is what settled
it. `sha2` is now the **first third-party runtime dependency in the
workspace**, and the rule that replaces "no dependencies" is narrower and
better: *take one where being wrong would be silent.* A mistyped CLI flag,
a malformed manifest and a corrupted base64 all fail loudly and stay
hand-written; a wrong hash selects the wrong dictionary and decodes to
plausible nonsense.

**A store that cannot file a dictionary under the wrong hash.** `insert` takes
bytes and computes the identity, so the alternative shape — insert with a
caller-supplied hash and verify later — has no way to exist, and neither does
its failure mode of a skipped verification silently decoding against the wrong
dictionary.

**The bug, and which test caught it.** The hand-written streaming SHA-256
dropped buffered bytes whenever an update ended without completing a block:
`update` fell through to a tail ending `self.buffered = rest.len()`, which is
zero once the input is exhausted. Every one-shot NIST vector passed it, because
a one-shot call never re-enters with a partial buffer. The 1-million-`a` vector
passed it too, because 1 000-byte updates never leave the buffer short. It took
the test that hashes every length from 0 to 130 one byte at a time and compares
against the one-shot.

**Standard test vectors verify the algorithm, not the implementation's
plumbing.** The vectors are all a single call; the bug was entirely in the
second one. That is the whole case against hand-rolling a hash: the part with a
published conformance suite was never the risky part, and `block-buffer` exists
because the risky part is a separate problem somebody has already solved. The
every-length test is kept, now worth little, as a note to whoever next thinks
the vectors are sufficient.

Three end-to-end checks, because the unit tests only prove the parser agrees
with itself: our SHA-256 against the system's `sha256sum` over 40 KB of real
bytes (the same second-oracle reasoning that put `ruzstd` in the codec's
dev-dependencies); the reference decoding a complete `dcz` stream **header and
all**, which is the wire-compatibility claim the format rests on; and zalt doing
the same, then resolving the dictionary by hash and decoding byte-exactly. A
fourth test compresses against one dictionary and decodes against another to
show the codec *cannot* detect the substitution — evidence for why the hash
exists, rather than an assertion of it.

#### 5.3M executions that tested almost nothing

The first `dcz` fuzz target fed arbitrary bytes to the header parser, survived
5.3M executions, and reported no findings. That number is worthless, and the
reason is arithmetic: **the header opens with a 64-bit fixed sequence**, so raw
bytes reach the accepting path with probability 2^-64. Essentially every
execution took a rejection branch. The branch that extracts a hash and splits a
frame — the one containing arithmetic — was never reached at all.

`roundtrip_valid.rs` has recorded this lesson for frames since M1: *"Random
bytes essentially never form a valid zstd frame past the magic number."* It
applies at least as hard to a header, and the new target was written without
noticing. A fuzzer that cannot construct the shape under test measures the
mutator, not the code.

Both `dcz` targets are now structured:

- **`dcz_prelude`** takes a variant tag from the first byte, so roughly three
  executions in eight carry a genuine header — plus a header cut short at every
  length below 40, the right magic with a wrong length field, a `dcb` header, a
  magic one bit off, and a fully arbitrary remainder so nothing is closed off.
  It asserts sniffing and declared-`dcz` parsing never disagree, that the frame
  handed back is exactly the input minus forty bytes, and that a header built
  from known parts parses back to those parts.
- **`dcz_roundtrip`** is the one worth having. The fuzzer's bytes become a
  dictionary and a payload, libzstd compresses one against the other, and we
  rebuild the stream an origin would send. Every mutation changes the dictionary
  content, which changes which matches the encoder finds, which changes the
  offsets reaching back past the start of the frame — the dictionary-relative
  decode path, which arbitrary input would never construct. It checks the round
  trip, that the store files a dictionary under the hash the wire names it by,
  that the split is not off by one, and that decoding the whole stream *through*
  the header matches decoding the frame alone.

The difference is visible in the coverage counters rather than in the run
counts. Seeded, the header parser reports `cov: 134` — which is most of a small
parser — while the round trip starts at `cov: 966 ft: 1899` and climbs. 5.2M
executions of the first and 68K of the second, no findings in either; the
smaller number is the more informative one, because each of those executions
built and decoded a real dictionary-compressed frame.

`ci/seed-fuzz-corpus.sh` seeds both, and now documents the input shape each
target expects alongside the three that were already there.

#### Order-N, co-locality, and a caution for M5's block splitting

Recorded because the question will be asked again. **Context modelling is not
available to us.** zstd Huffman-codes literals with one table per block and no
context at all, unlike brotli, which selects a literal context mode from the
previous two bytes. Adding it is a bitstream change, which §7's table puts
permanently outside what a recipe may do.

The raw-byte entropy gap looks enormous — H₀ 4.60, H₁ 3.21, H₂ 1.73, H₃ 0.88
bits/byte on `antikythera.zdoc` — and is a trap twice over. Model cost is
excluded (order-3 has 2 318 contexts over 9 KB; that model cannot be shipped),
and it is entropy of the *raw* bytes, while LZ77 already removes most of the
repetition order-2 is detecting and zstd entropy-codes only the literals that
survive matching. The gap overstates the opportunity by a wide margin.

Co-locality does pay, in three wire-compatible places: dictionary ordering
(measured above, 3.7%), pre-computed entropy tables in a formatted dictionary,
and block splitting.

**The caution.** Splitting at a fixed 1 KB stride buys 2.4–5.1% of literal
entropy *before* paying for the extra tables, and a Huffman table is 100–200
bytes — eight extra tables on a 9 KB document costs ~1 200 bytes against a
~2 500 byte output. Fixed-stride splitting is a clear loss. §7 assumes markup
and hash regions can be separated into different blocks, but in a real `.zdoc`
the hashes are **interleaved at roughly 64-byte granularity** — every
`<img hash="sha256:…">` — not segregated into regions. The 2.4–5.1% is an upper
bound for any scheme at that granularity, and it sits below the table cost. M5
should measure the region structure before building the splitter, not after.

#### `zalt-recipe-train`, and a level that was sketched rather than measured

The last M4 box, and the smallest, because the measurement from `zalt-ztml`
already decided what it should not do: `zstd --train` loses to plain
concatenation on five of six documents at this corpus size, so v1 implements
the concatenation *properly* — ordering, size budget, held-out evaluation —
rather than implementing COVER against a corpus too small for it to beat `cat`.
`sweep` is the harness that would demonstrate the upgrade when a corpus large
enough for it arrives.

**It replaces `ci/gen-ztml-dict.py`, and reproduces its output byte for byte.**
Same 46 999 bytes, same
`698fce7d…` — a rewrite in a different language landing on the identical
artifact, which is a better argument for the generator being deterministic than
any assertion about it.

**The sweep found the manifest's compression level was a sketch.** §7's example
said `level = 6` and `zalt-ztml` copied it. Held out, at full dictionary size:

| | L3 | L6 | L9 | L12 | L19 |
|---|---|---|---|---|---|
| **mean gain** | 23.5% | 27.2% | **29.0%** | 29.5% | 30.8% |
| **encode cost** | 1.0x | 1.5x | **1.7x** | 5.1x | 15.2x |

L9 is the knee: **1.8 points better than L6 for 13% more encode time**, where
L12 costs three times L9 for a further half point and L19 nine times for 1.8.
The manifest now says 9. The asymmetry that settles it is that encode cost is
paid once per resource by whoever publishes it, and ratio is paid by every
reader.

**Dictionary size dominates level, and has a sharp knee of its own.** Held-out
mean gain by size, at L9: 4 KB 7.2%, 8 KB 12.0%, **16 KB 23.8%**, 32 KB 28.0%,
47 KB 29.0%. The step from 8 to 16 KB nearly doubles the gain, because 16 KB is
where a whole sample document first fits; past 32 KB the last third of the
dictionary buys one point. A host on a binary budget has the numbers to trade
with, and `--max-size` trims **from the front**, since the tail is where the
cheap offsets are.

**A harness bug that inflated every number by a point.** The first version
piped documents to `zstd` through stdin. That leaves the input size unknown, so
the frame carries no `Frame_Content_Size` and the encoder cannot size its
choices to a resource it has not seen the end of — worth 27–44 bytes, and worth
them almost entirely to the *baseline*, because the dictionary case is dominated
by long matches either way. Every gain came out about one percentage point too
high, from a detail with nothing to do with dictionaries. The tool now hands
`zstd` a path, as a server compressing a resource it holds would. The
discrepancy was caught only because the earlier bash figures existed to
disagree with it: **a second measurement of the same thing is worth more than a
more careful single one.**

**And the clearest demonstration of why leave-one-out is not optional.** The
shipped dictionary contains `showcase.zdoc` verbatim, so compressing that
document against it yields **23 bytes from 7 782** — one match covering the
whole file. Every number in this document is leave-one-out; the dictionary is
rebuilt without the document being measured, which is N times more expensive
and is the difference between 29% and a figure nothing in production will ever
reproduce.

### M5 — Encoder

Staged, because "the encoder" is several separable pieces. The ordering is the
one this section asked for: correctness, then ratio. Every ratio box below was
opened only after the differential harness against libzstd and `ruzstd` was
already passing, so no increment could trade correctness for size without
something saying so.

- [x] **Frame layer and stored blocks.** `encode/` writes frame headers, raw and
      RLE blocks, block splitting by size, and the content checksum. Complete,
      wire-valid zstd at a ratio of 1.0 — which is not a placeholder but the
      `strategy = "stored"` path §7 specifies and `zalt-ztml` already declares.
- [x] `encode_decode` differential fuzzing. 3.4M executions, no findings.
- [x] Parameter plumbing from recipes: `Recipe::codec_params` applies routing
      and `[[rule]]` overrides and hands back an `EncodeParams` the codec takes.
- [x] **Compressed blocks**: raw literals, predefined FSE tables, greedy match
      finder. 1.9x-7.1x, which is 1.3x-1.5x larger than `zstd -1`.
- [x] **Huffman literals**, with the FSE-compressed weight description.
      Gap to `zstd -1` closed from 1.29x-1.50x to **1.09x-1.46x**.
- [x] **Fitted FSE tables** for the sequence codes. zalt now **matches
      `zstd -1`** on small documents; the gap that remains is the match finder.
- [x] **Match finder**: hash chains, lazy matching, history across blocks.
      zalt now sits **between `zstd -3` and `-9`** on every file measured.
- [x] **Repeat offsets**, with the decoder's own rule as the oracle. Worth
      0.1%-1.4%, rising with file size.
- [x] **Encoder throughput**, measured properly at last, and then worked on:
      **1.58x**, with the output byte-identical at every step. The guess about
      where the time went — the three build-both-and-compare choices — was
      wrong, and the profile said so in one run.
- [x] **Block splitting** at statistical transitions (§7) — built as a
      *detector*, after the measurement below contradicted the premise. Worth
      1.6% on separable content and exactly nothing, at no cost, on everything
      else.
- [x] **Compression levels 1–19**, after the correctness and throughput work
      above, as this list asked. Level 19 lands within **0.2%** of `zstd -19`
      on the ZTML documents and **0.3%** on large input, from 1.7% and 64%, at
      1.1x its time on large input once a binary-tree finder replaced the
      chain for 16–19. The M5 configuration survives as an exact fixed point.
      Levels 20–22 deferred deliberately — see below. `zalt compress` exposes
      them at the shell.
- [ ] Ratio work only after correctness.

#### The stored encoder, and three decoders

An encoder cannot be checked by reading its output; the only question worth
asking is whether somebody else can read it. So the differential harness was
wired up before a byte of ratio work, against three decoders with deliberately
different standing:

1. **libzstd**, through its CLI. Its refusal is our bug, with no second opinion
   to appeal to — the reference *defines* what the ecosystem accepts.
2. **`ruzstd`**, independent and pure Rust, so a misreading shared between
   zalt's encoder and zalt's decoder cannot hide behind itself.
3. **zalt's own decoder**, the weakest of the three, and there to catch the two
   halves drifting apart rather than to establish anything.

All three read every frame the encoder emits, across a matrix of content sizes
chosen to straddle each header field boundary and six parameter sets.

**The writer is the parser's inverse, and is tested as that rather than against
the prose.** `crate::frame` has been read against the reference corpus since M1,
so the header writer was derived from *it*; the test asserts a round trip —
anything written must parse back to what it was written from — rather than
asserting a byte layout that would just be the RFC transcribed a second time,
with whatever second misreading that invites. §4 records one the prose already
caused.

**One asymmetry caught by writing the inverse.** `Frame_Content_Size_flag` 0
means a *one-byte* field when `Single_Segment` is set and **no field at all**
when it is not. A writer that treats the flag as size-only emits a byte the
parser will not read, and the frame desynchronises at the first block header.
The first draft here did exactly that; the parser's own comment is what caught
it, which is the argument for deriving one from the other.

**`Strategy::Compress` returns `Unimplemented` rather than falling back to
stored.** Handing ratio-1.0 output to a caller who asked for compression is the
invisible failure this document keeps recording: the bytes decode, nothing
errors, and the only symptom is a ratio nobody compared.

#### Compressed blocks, and the two oracles that made them safe

The plan in the previous section held: **raw literals plus predefined FSE
tables needs no entropy analysis at all**, so the first compressed block was a
match finder, an FSE encoding table, and a backward bitstream writer. Measured
with `just encode-ratio`:

| file | raw | zalt | zstd -1 | gap |
|---|---|---|---|---|
| `showcase.zdoc` | 7 782 | 4 031 (1.93x) | 3 115 (2.50x) | 1.29x |
| `handsome.zdoc` | 11 755 | 5 600 (2.10x) | 4 303 (2.73x) | 1.30x |
| `rfc9842.txt` | 35 626 | 15 041 (2.37x) | 11 452 (3.11x) | 1.31x |
| `synthetic.zdoc` | 219 355 | 30 718 (7.14x) | 20 511 (10.69x) | 1.50x |

Behind the reference by about a third, which is what raw literals cost: every
literal is a byte on the wire where libzstd Huffman-codes it, and §8 says this
content is literal-dominated. That is the next increment and the gap is the
argument for it.

**Three deliberate simplifications, each removing a class of bug rather than
saving work.** Matching stays *inside the block*, so there is no history buffer
to keep in step with the decoder's. **Repeat offsets are never emitted** —
`Offset_Value` 1-3 are simply not used — so the encoder never models the
`literals_length == 0` update that §4 calls "the single most commonly botched
part of the format". And a block falls back to stored whenever the compressed
form is not smaller, without which "compress" would reliably *expand* exactly
the incompressible data `strategy = "stored"` exists to protect.

**Codes are derived from the decoder's tables, not transcribed.** libzstd
carries `LL_Code[64]` and `ML_Code[128]` mapping a value to its code;
transcribing those would be two more chances at the mistake §10 records against
`ML_DEFAULT`, where a table that summed correctly still mapped states to the
wrong symbols with every self-consistency check passing. So a value's code is
searched out of the baseline tables the decoder already uses. The two
directions cannot disagree about a table they share, and the tests assert the
composition rather than the contents.

**The bug the tests caught, and what kind it was.** The match finder bounded a
match's source slice at the current position, which caps every match at its own
offset and therefore forbids **overlapping matches entirely** — the offset-1 run
that is §5's win #2 could never be found, because its source is one byte long.
Every frame produced was still valid and every decoder still read it; the only
symptom was ratio. It was caught by an assertion that a run of one byte must
produce an offset-1 sequence, written because the case was interesting, not
because anything suggested it was broken. **A correctness suite would not have
found this**, and neither would the fuzzer, whose oracle is agreement rather
than size.

#### Huffman literals, and the header that nearly made them pointless

| file | zalt before | zalt now | `zstd -1` | gap before | gap now |
|---|---|---|---|---|---|
| `showcase.zdoc` | 4 031 | **3 410** | 3 115 | 1.29x | **1.09x** |
| `handsome.zdoc` | 5 600 | **4 876** | 4 303 | 1.30x | **1.13x** |
| `rfc9842.txt` | 15 041 | **13 952** | 11 452 | 1.31x | **1.22x** |
| `synthetic.zdoc` | 30 718 | **30 001** | 20 511 | 1.50x | **1.46x** |

**The direct weight description is unusable on real data, and the reason is not
the one it looks like.** A Huffman table is described by a weight per symbol,
and the cheap form writes them four bits each behind a header byte of
`127 + count` — so it tops out at 128 weights. That sounds generous for text
until the count turns out to run to the largest *symbol value*, not the number
of distinct symbols. Every file in this corpus uses fewer than a hundred
distinct bytes and still reaches byte 194 or beyond, so not one of them can use
it. The measurement is worth keeping because the limit reads like a
128-distinct-symbol limit and is not:

| file | distinct bytes | largest byte |
|---|---|---|
| `showcase.zdoc` | 87 | 226 |
| `rfc9842.txt` | 90 | 239 |
| `synthetic.zdoc` | 80 | 194 |

So the FSE-compressed weight description had to be written too — which meant an
FSE **table description** writer, a count normaliser, and the two-state
interleaved coder, none of which the sequence section needed because predefined
tables carry no description at all.

**Two bugs, both found by round-tripping through our own decoder.**

The first was in length limiting. zlib's repair — clamp the overlong leaves,
then pay the surplus back two at a time — assumes the surplus is exactly
`overflow / 2` slots, which holds when depth is limited *during* construction
and not when a tree is built unlimited and clamped afterwards. Kraft came out
wrong and `build` refused its own table. Replaced with direct accounting of the
sum, which is slower, has no precondition, and can be asserted.

The second is the more interesting one. The interleaved FSE coder started both
states blank; libzstd **seeds** them from the last two symbols, which emit no
bits. Starting blank makes the stream one symbol too long, and the decoder —
which stops on exhaustion rather than on a count — recovers an extra weight.
The symptom was that every literal decoded correctly *except the highest
symbol*, which came back as its neighbour: one extra weight shifts the whole
weight array, and so the whole table, by one. `'z'` decoded as `'|'`. A
correctness bug that presents as a single wrong byte in a thousand is exactly
the kind this codec's differential harness exists for, and a round trip through
our own decoder was enough to expose it.

**Where the rest of the gap is.** `synthetic.zdoc` barely moved, because at
7.3x it is match-dominated and has few literals to code. The sequence codes are
still on the *predefined* FSE distributions, which is what remains — fitting
tables to each block's own statistics, with the table-description writer this
increment already had to build.

#### Fitted FSE tables, and where the encoder now stands

| file | raw | zalt | `zstd -1` | `-3` | `-9` | `-19` |
|---|---|---|---|---|---|---|
| `showcase.zdoc` | 7 782 | **3 087** | 3 115 | 3 044 | 2 910 | 2 885 |
| `rfc9842.txt` | 35 626 | **11 458** | 11 452 | 10 952 | 10 066 | 9 716 |
| `rfc8878.txt` | 112 425 | 29 827 | 28 362 | 27 006 | 24 554 | 23 215 |
| `synthetic.zdoc` | 219 355 | 22 996 | 20 511 | 18 108 | 10 766 | 8 253 |

**zalt matches `zstd -1` on small documents**, and beats it on `showcase.zdoc`
by 28 bytes. That is the level and the file size §8 says this workload is: many
small resources, where per-frame cost dominates. It is not a claim about zstd in
general — `-3` is 1–2% ahead on those files and much further ahead on the large
one.

The progression is worth keeping, because it says which work bought what:

| | `showcase.zdoc` | gap to `-1` |
|---|---|---|
| raw literals, predefined tables | 4 031 | 1.29x |
| + Huffman literals | 3 410 | 1.09x |
| + fitted FSE tables | **3 087** | **0.99x** |

**Both candidates are built and the smaller kept**, rather than choosing from an
entropy estimate as libzstd does. An estimate must model the table
description's cost as well as the bitstream's, and being wrong is invisible —
it costs ratio, silently, which is the failure this encoder has now tripped over
twice. Encoding the section twice is a plain cost for an answer that cannot be
worse than the predefined tables it replaces. The refinement left is choosing
*per table* rather than for all three at once: the modes byte has independent
fields, so a block could fit its literals lengths and keep the predefined
offsets. Eight candidates instead of two, and worth doing when a measurement
asks for it.

**What is left is the match finder, not the entropy coder.** `synthetic.zdoc` is
where the gap is widest, and it is 219 KB — two blocks, so every match that
would reach across the boundary is unavailable, and within a block the parse is
greedy with no repeat offsets. Those three simplifications were taken
deliberately in the previous increment and each removed a class of bug; they are
now the whole remaining difference. Entropy coding is done.

#### The match finder, and where the encoder ended up

| file | raw | zalt | `zstd -1` | `-3` | `-9` | `-19` |
|---|---|---|---|---|---|---|
| `showcase.zdoc` | 7 782 | **2 922** | 3 115 | 3 044 | 2 910 | 2 885 |
| `rfc9842.txt` | 35 626 | **10 220** | 11 452 | 10 952 | 10 066 | 9 716 |
| `rfc8878.txt` | 112 425 | **25 448** | 28 362 | 27 006 | 24 554 | 23 215 |
| `synthetic.zdoc` | 219 355 | **11 904** | 20 511 | 18 108 | 10 766 | 8 253 |

**Better than `zstd -3` on every file, and within a few percent of `-9`.**
`synthetic.zdoc` moved the furthest — 22 996 to 11 904, very nearly halved —
because it is the only file large enough to have a second block, and cross-block
history is what it had been missing.

The whole progression, on the file that started this:

| | `showcase.zdoc` | versus `zstd -1` |
|---|---|---|
| raw literals, predefined tables | 4 031 | 1.29x |
| + Huffman literals | 3 410 | 1.09x |
| + fitted FSE tables | 3 087 | 0.99x |
| + chains, lazy, cross-block | **2 922** | **0.94x** |

**Three changes, and the one that mattered most was not the clever one.** Hash
chains and lazy matching are the textbook improvements; carrying the matcher
across block boundaries is bookkeeping. On a corpus of small files the first two
did most of the work, and on the only large file the third did nearly all of it.

**The bug, which cost every match in the file.** The parse inserted each
position into the hash chain *before* searching from it — so the search's first
candidate was the current position, the walk stopped immediately on "not
earlier", and no match was ever found. The failing assertion was the offset-1
run, the same case that caught the previous match-finder bug. The fix carries a
loop invariant now: every position below the cursor is inserted **exactly
once**, and none at or above it. Both halves are load-bearing, and the second is
the subtler one — inserting a position twice points its chain slot at itself,
and the walk from there is a self-loop that only the descending-chain guard
stops.

**A test that passed for the wrong reason.** The first cross-block test
compressed repeated prose and asserted the second block reached back into the
first. It failed — correctly. With prose repeating every 44 bytes there is
always a nearer match, so the matcher was right to take one and the *assertion*
was wrong. Rewritten over content with no internal repetition, where the only
match available is a block away, it tests what it claims to.

#### Repeat offsets, and asking the decoder rather than inverting it

The last simplification, and the one §4 singles out: *"the repeat-offset update
has a special case when `literals_length == 0`. The single most commonly
botched part of the format."*

**So it was not inverted.** `crate::sequences::resolve_offset` — read against
the reference corpus since M1 — is now `pub(crate)`, and the encoder *asks* it:
for each sequence it tries offset values 1, 2 and 3 against a copy of the repeat
table and takes whichever reproduces the offset it wants, falling back to a
literal `offset + 3`. The commit goes through the same function. The two
directions cannot disagree about a rule they share, and the case §4 warns about
is never written down twice.

| file | before | after | saved |
|---|---|---|---|
| `showcase.zdoc` | 2 922 | 2 920 | 0.1% |
| `rfc9842.txt` | 10 220 | 10 188 | 0.3% |
| `rfc8878.txt` | 25 448 | 25 172 | 1.1% |
| `synthetic.zdoc` | 11 904 | 11 741 | 1.4% |

**Smaller than expected, and the reason is instructive.** The first version only
used a repeat when the longest match *happened* to sit at one, which is rare —
worth 0.1% to 0.5%. Most of the value is in the match *search*: a repeat costs a
code of 0 or 1 where a literal 128 KiB offset costs seventeen bits, so an
equal-length match at a recent offset is strictly the better sequence. Moving
the choice into the matcher and preferring repeats roughly doubled the saving.

Only "no shorter", deliberately. Trading match length for a cheaper offset is a
real trade and making it well needs a cost model this encoder does not have —
which is also why libzstd, whose optimal parser prices every candidate, gets
more from repeat offsets than this does.

**One trap worth recording.** The repeat table advances during parsing, but a
block whose compressed form does not pay is emitted **stored** — and a stored
block advances nothing on the decoder's side. Leaving the encoder's table ahead
of the decoder's would mistranslate every later repeat code, producing wrong
bytes with no error anywhere. So the table is snapshotted before each block and
rolled back on fallback, and a test asserts the restore is exact. Nothing found
this; it was noticed while writing the fallback, which is the kind of bug that
would otherwise have waited for a corpus with one incompressible block in the
middle of a compressible file.

#### Block splitting: the measurement changed what got built

The M4 caution asked for §7's premise to be checked before a splitter was
written. Checking it turned a splitter into a detector.

**Splitting at a fixed stride is a loss on every real file.** Compressed size
falls monotonically as blocks grow:

| block size | 4 KiB | 8 KiB | 16 KiB | 32 KiB | 64 KiB | 128 KiB |
|---|---|---|---|---|---|---|
| `rfc9842.txt` | 10 751 | 10 483 | 10 317 | 10 258 | **10 188** | **10 188** |
| `rfc8878.txt` | 26 584 | 25 764 | 25 303 | 25 153 | **25 150** | 25 172 |
| `synthetic.zdoc` | 14 261 | 13 048 | 12 338 | 12 007 | 11 862 | **11 741** |

Every block pays for a Huffman weight description and three FSE table
descriptions, and smaller blocks give those tables less data to fit against.
The 22 bytes by which 64 KiB beats 128 KiB on `rfc8878.txt` is 0.09% and not a
finding.

**The mechanism is real; the content is what is missing.** A controlled pair
settles it — the same bytes, arranged two ways:

| content | one block | split at the boundary | delta |
|---|---|---|---|
| 64 KiB text, then 64 KiB random | 83 656 | 82 340 | **-1.6%** |
| the same, interleaved every 64 bytes | 87 267 | 87 173 | -0.1% |

Separable content gains meaningfully. Interleaved content — which is what a
document carrying a hash per subresource *is*, and what the M4 caution said
`.zdoc` looks like — gains nothing. §7's paragraph assumed markup and hash
regions could be put in different blocks; they cannot, because they alternate
every sixty-four bytes.

**So the splitter detects rather than splits.** `encode::split::transitions`
proposes a boundary only where the byte distribution actually changes, measured
as L1 distance between adjacent 4 KiB histograms — integer arithmetic, because
entropy would want a logarithm and `core`, being float-free, has none. When it
proposes nothing, which is the answer for every file in this corpus, the
encoder does not encode anything twice and the feature costs nothing at all.
When it does propose something, both arrangements are encoded and the smaller
kept, so the fixed-stride result is always the floor.

> **"Costs nothing at all" is wrong, and was measured at about 3%** during the
> throughput work below. Nothing is encoded twice, but the detector is itself a
> full histogram pass over the input before it can say so.

That is the third place in this encoder where a candidate is built and measured
rather than chosen by estimate — after the literals section and the sequence
tables. The pattern is deliberate: every one of those choices is invisible when
wrong, costing only ratio, and this document has recorded enough of those to
stop guessing at them.

#### Encode cost, measured the way §10 insists

Ratios are deterministic and can be quoted directly. Time cannot, and this
machine proved it again: the same measurement of `rfc8878.txt` gave 0.37 s and
then 0.17 s in consecutive runs. Only interleaved, best-of-three comparisons
mean anything, and even those are held loosely.

On that basis zalt's encode costs roughly **1.1x-1.4x `zstd -9`** while
producing output between `-3` and `-9`. That is a defensible place to be and it
is not a claim to build on: the encoder has had no throughput work at all, and
carries at least one obvious waste — the sequences section is built twice, once
against the predefined tables and once against fitted ones, to compare their
sizes. Anyone acting on encoder speed should measure it properly first, against
the discipline in §10 rather than against the numbers here.

> **The paragraph above took its own advice and did not survive it.** Measured
> properly, the figure was **3.5x `zstd -9`**, not 1.1x-1.4x. The next section
> is what the measurement actually said.

#### Encoder throughput: the harness, and what it found

The box above asked for this to be measured before anyone acted on it, and the
first thing measuring it did was refute the number beside the request. Both
halves of that are worth keeping, because the estimate was not idly made — it
came from timing whole files with a stopwatch on a machine §10 has twice caught
lying — and it was wrong by a factor of two and a half.

**`zalt-bench encode` is the harness**, and it differs from the `decode` mode in
two ways that were the whole reason the old figure was unreliable:

- **libzstd is linked in, not driven through its CLI.** The decode mode's own
  comment concedes its reference column "is not comparable at small sizes"
  because it carries a fork and two pipes. For encode that is not a caveat but a
  defect: §8's workload is many small resources, and a 3 KB file would have been
  mostly measuring `posix_spawn`.
- **The rivals alternate batches inside one loop.** Running one implementation
  to completion and then the other puts them in different windows of wall-clock
  time, and on this machine that is not a detail. Interleaving does not make the
  machine quieter; it makes the interference *shared*, so a thermal step moves
  both columns and leaves the ratio where it was. The ratio is what gets quoted,
  so the ratio is what has to be defended.

The headline is a **gap ratio** rather than MB/s, for the reason §5 gives about
corpora: a rate off this laptop is a number about the laptop. And both `-3` and
`-9` are timed, because zalt's output sits between them and quoting one level
alone picks the flattering half.

**The starting point, alternating against the finished encoder on the same
corpus:**

| | gap to `zstd -3` | gap to `zstd -9` |
|---|---|---|
| M5 as it stood | 16.49x | 3.51x |
| after this work | **10.36x** | **2.22x** |

**1.58x, and every byte of output is unchanged** — `showcase.zdoc` is still
2 920, `rfc8878.txt` still 25 172, `synthetic.zdoc` still 11 741. That was the
constraint the whole pass was run under, and it is also what made the A/B
trustworthy: two binaries that emit identical bytes differ in exactly one thing,
so a difference between them is not a different encoder being measured.

On the small `.zdoc` files that §8 says are the dominant case, zalt now encodes
**faster than `zstd -9`** — 0.56x-0.68x — while producing smaller output than
`-9` on several of them.

#### The profile, and the guess it replaced

The open box guessed that the time was in the three places this encoder builds
both candidates and keeps the smaller. One run of `just profile-encode` said
otherwise, and it was not close:

| | 112 KB text | 7.7 KB `.zdoc` |
|---|---:|---:|
| `Matcher::find` | **70%** | 37% |
| `block::write_compressed` (the parse loop, inlined) | 12% | 26% |
| `CState::encode` + `write_values` (the sequence bitstream) | 11% | 14% |
| `memset` (the matcher's own tables) | <1% | **7.6%** |

The build-both waste is the third row, and it is real — but it was never going
to be more than a tenth, and the match finder was two thirds. **This is the same
shape of error §5 records against its own ranked list**: a plausible ordering
derived by reasoning, never checked against a profile, wrong about which end of
the program the time was at.

Four changes carried the 1.58x:

- **`common_prefix` a word at a time.** The innermost loop of the encoder, run
  up to `SEARCH_DEPTH` times per position and twice over for the lazy step, was
  a byte-at-a-time `take_while` — a compare and an unpredictable branch per
  byte. Eight bytes at once is an XOR and a `trailing_zeros`: `eor`/`rbit`/`clz`
  on Firestorm, three instructions for what was eight iterations. It needs no
  `unsafe` and no bounds check either, because `first_chunk` returning `None` is
  the loop's exit condition.
- **Rejecting a candidate on one byte.** A candidate is only worth comparing if
  it is *longer* than the match in hand, so it must agree at the byte where that
  match ends. Most candidates fail it — they share a four-byte hash and little
  else — and the effect compounds, because the longer the best match grows the
  cheaper each remaining candidate is to dismiss. **Selection is provably
  unchanged**: a candidate that agrees up to `best_len` and then differs *ties*,
  and a tie never passed `length > best_len` either. The same rejection was
  worth having on the three repeat-offset probes, which run for every position
  that finds a match.
- **Sizing the matcher's tables to the input.** A 2^17 hash table and a chain of
  the same order is about a megabyte zeroed per frame, and §8's workload pays
  that once per resource on resources of a few kilobytes — 7.6% of a 7.7 KB
  encode spent clearing slots no position can reach. The chain cap is free of
  consequence rather than a trade, since a chain at least as long as the input
  never has two live positions sharing a slot.
- **Deferring the bit writer's flush.** `bitstream.rs` said in its own comment
  that flushing after every field was the safe choice and that deferring it was
  "the change to make when there is a measurement asking for it". This was that
  measurement. **The invariant taken is local**, which is the point: a field is
  at most 32 bits, so flushing whenever `bit_pos` exceeds 32 leaves at most 32
  behind and the next field cannot overflow the container — provable from the
  assertion three lines above it, not from a bit budget every caller has to
  maintain. The bytes are identical; only when a complete byte leaves the
  container changes.

#### Two rearrangements that measured nothing

Both are recorded because the reasoning behind them was good and the reasoning
was not the point.

`Matcher::find` is a **pointer chase**: each candidate's address comes out of
the previous candidate's chain slot, so the walk runs at one dependent load per
step and nothing can prefetch it. §5 says Firestorm "rewards *exposing
independent dependency chains* far more than clever vectorization of a serial
loop", and the encoder has two such chains available at every position, because
the lazy step searches `pos + 1` whatever `pos` found. So:

1. **The two walks were interleaved**, stepping alternately to put two misses in
   flight instead of one.
2. **The lookahead was carried forward.** When `pos` finds nothing the parse
   advances one byte and searches `pos + 1` — the position the lazy step just
   examined, against a chain that changed in no way that walk could see. That is
   a real halving of the searches on literal-heavy content, not a latency trick.

A three-way alternating A/B put all three variants inside run-to-run noise:

| | gap to `-9`, best of three | gap to `-3`, best of three |
|---|---|---|
| plain | 2.43x | 11.70x |
| carried lookahead | 2.43x | 11.46x |
| interleaved pair | 2.51x | 11.40x |

**Neither is kept.** The interleave needs the insert to lead the search, which
is exactly the bug this parse has already had once — the chain walk stopping on
`pos` itself and never finding a match — and re-admitting that ordering behind a
skip is a liability with no payoff on the other side of it.

The likely reason the premise failed is written down one section up, about a
different feature. §5 struck match-source prefetching at M3 on the finding that
"a 2 MiB window — and even the 8 MiB browser ceiling — sits inside the M1
P-cluster's 12 MiB shared L2, so the load is already ~15 cycles and an
out-of-order core hides it without help." A 128 KiB window's hash and chain
tables are far smaller again. **The walk was never missing cache**, so there was
no latency to overlap, and the second miss put in flight was a miss that did not
exist. That argument was available before either change was written, in this
document, about this machine.

#### A claim in the section above, corrected

§10's block-splitting notes say that when the detector proposes nothing —
the answer for every file in this corpus — "the encoder does not encode anything
twice and the feature costs nothing at all."

The first half is true and the second is not. `split::transitions` is a full
histogram pass over the input before it can conclude there is nothing to
propose. Alternating a build with the detector against one that returns
immediately puts it at **about 3%** of encode time, for no change in output on
any file here.

That is not an argument for removing it — it pays 1.6% on content that is
genuinely separable, and 3% of encode is a fair price for a ratio win the
encoder cannot otherwise get. It is an argument for not writing "costs nothing
at all" about a pass over the input, which is the kind of claim that is only
ever checked when somebody goes looking for time.

#### What is left, and what the numbers do not say

The remaining gap is still overwhelmingly `Matcher::find`, at about 58% after
this work. Closing it means changing what the search *does* — a better hash, a
bounded tree, a cost model for the depth budget — rather than making the current
search faster, and all of those change the output, so each needs the ratio table
defended as well as the throughput one. That is a different kind of increment
from this one and should not be started by assuming it wins.

**Two things this measurement exposed that are not throughput**, recorded here
because nothing else in §10 would catch them:

- **`zstd -3` beats zalt on ratio on large, varied input.** On 4 MB of mixed
  source text zalt produces 662 766 bytes against `-3`'s 510 872 — 30% larger.
  *(Fixed by the compression levels below, and the cause was one parameter:
  the window was 128 KiB at every size.)*
  M5's "better than `zstd -3` on every file measured" is true of the files it
  measured, all of which are documents under 220 KB. The claim should not be
  carried past that corpus.
- **The corpus used here is not the ZTML corpus.** It adds the two RFCs and a
  4 MB slice of mixed source text, because a throughput measurement over six
  files of 3-12 KB is dominated by per-frame setup and says nothing about
  steady state. §5 is right that corpus choice is a recipe concern; that cuts
  both ways, and a throughput corpus and a ratio corpus are not the same
  document set.

#### `ci/interop.sh`, and why it is not the same evidence as the tests

`tests/encode.rs` links our encoder and our decoder into one binary and calls
libzstd through its CLI. `just interop` runs the actual `zstd` executable over
files on disk, in four directions — zalt to zstd, zalt to `ruzstd`, zstd to
zalt at four levels, and zalt to zalt — across both strategies, with and
without a checksum, including an empty file and a single byte. 112 checks —
322 since the compression levels, which added one level from each way of
parsing (1, 5, 9, 16, 19) to the encoding side.

Two decoders and not one, for the reason §9 gives for `ruzstd` on the decode
side: libzstd is the reference and its refusal is our bug, but a *shared*
misreading between our encoder and our decoder would be invisible to a round
trip, and `ruzstd` shares code with neither. The in-process tests assert the
same properties; the script is what an ecosystem would actually do with our
bytes, and if the two ever disagree the disagreement is the finding.

#### Compression levels, 1 to 19

Until here `EncodeParams::level` was carried for the recipe plumbing and
ignored: one configuration — depth-32 hash chains, one lazy step, a 128 KiB
window — served every request. The ZTML recipe has declared `level = 9` since
M4 and received that configuration regardless.

**The level-against-level baseline**, from the new `zalt-bench encode-levels`,
which compares zalt at level *N* to libzstd at level *N* on size and on time,
both as zalt divided by zstd, so that below 1.00 is better:

| corpus | zalt (every level) | `zstd -1` | `-3` | `-9` | `-19` | zalt against `-19` |
|---|---:|---:|---:|---:|---:|---:|
| ZTML documents (6, 44 KB) | 17 261 | 18 239 | 17 895 | 17 153 | 16 970 | 1.017x |
| large (3 files, 4.5 MB) | 699 679 | 776 929 | 555 978 | 474 129 | 426 296 | **1.641x** |

On §8's workload there was almost nothing to win — libzstd's *entire* range
spans 7% on these documents, and zalt already sat inside it. On large input
zalt was 64% behind `-19` and 26% behind `-3`.

**One parameter was most of that.** Before designing anything, the window alone
was varied on the large set: 17 → 699 679 bytes, 19 → 623 978, **21 → 487 672**,
and no further gain at 22 or 23. A 128 KiB window on multi-megabyte input was
30% of the output, and is the whole cause of the "`zstd -3` beats zalt on large
input" finding above.

##### The table, and what a window now means

`encode/level.rs` maps each level to a window, a hash size, a search depth and a
way of choosing: **greedy** (1–3), **one-step lazy** (4–6, the M5 behaviour),
**two-step lazy** (7–15) and **optimal parsing** (16–19). The *shape* follows
libzstd's `clevels.h`, read from the vendored 1.5.7 source rather than recalled.
The *values* were set by measurement against libzstd at the same level, because
zalt has one hash-chain finder where libzstd has six strategies, and the same
numbers do not mean the same work.

**`window_log` became a ceiling**, and its default moved from 17 to **23**.
The window is decoder memory, so it is the receiver's to bound: the parameter
now says how much history the other end has agreed to hold, the level picks a
window within it, and the frame declares no more than the content needs — a
10 KB document declares a 16 KB window at any level. 23 (8 MiB) is RFC 8878's
interop floor and §6's untrusted-decode ceiling, so a default frame is one
every browser accepts. The ZTML recipe's `window_log = 17` now caps it at
128 KiB, which its documents never approach. A test checks every level, from
−3 to 25, against every size class from empty to a gigabyte, for all three
bounds at once.

**Levels 20–22 are deferred, and the reason is §6.** libzstd's 20, 21 and 22
differ from 19 chiefly by declaring 32, 64 and 128 MiB windows — frames every
browser must refuse, from a codec whose first consumer is a browser. For now a
level above 19 is treated as 19, as libzstd treats a level above its own
maximum. When they are built, the natural form is the one libzstd's CLI
already uses: an explicit opt-in past the ceiling, not a level number alone.

**The M5 configuration is kept as a fixed point.** `Matcher::new` still builds
it, for the tests, and with a level temporarily pointed at the same knobs the
whole pipeline — window clamp included — reproduces the M5 table to the byte:
2 920, 10 188, 25 172, 11 741. The refactor changed nothing it was not asked to.

Levels 1–15, against libzstd's same level:

| level | parse | ZTML documents, size | large input, size |
|---:|---|---:|---:|
| 1 | greedy | **0.975x** | **0.972x** |
| 3 | greedy | **0.979x** | **0.960x** |
| 5 | lazy | 1.002x | **0.992x** |
| 9 | lazy ×2 | 1.005x | 1.013x |
| 12 | lazy ×2 | 1.009x | 1.017x |
| 15 | lazy ×2 | 1.013x | 1.029x |

**Speed is the honest half of the table.** Levels 9–15 *outrun* libzstd on the
ZTML documents (0.25x–0.76x its time) and level 15 is at parity on large input.
But **levels 1–3 are 4–6x slower than `zstd -1` to `-3`**: zalt runs at about
45 MB/s on small documents whatever the search depth, so the cost is per
position and per frame, not in the search. libzstd's `fast` and `dfast`
strategies skip work zalt's single finder always does — inserting every
position, building entropy tables per block — and closing that is its own
project, not a table entry.

**What changed for a caller who sets nothing.** The default is level 3, as in
libzstd, and level 3 is now greedy where every call used to get the M5
configuration. Alternating the two binaries, each against `zstd -3` in the same
run:

| default, level 3 | ZTML documents | large input |
|---|---|---|
| size | 17 511 against 17 261 — **1.4% larger** | 533 907 against 699 679 — **24% smaller** |
| time against `zstd -3` | 4.4x against 5.2x — **about 15% faster** | 5.3x against 10.7x — **twice as fast** |

So the default gives up 1.4% on small documents for speed and wins outright on
large ones. The ZTML recipe is unaffected in the direction that matters: it asks
for level 9, which now comes out *smaller* than the old configuration did
(17 240), so the browser's own frames improve.

##### Optimal parsing, for 16 to 19

The lazy parse takes the longest match it can find, a byte or two of lookahead
aside. An optimal parser instead prices *every* way of covering a stretch of
input — each literal, each length of each candidate match — and keeps the
cheapest path. What it can do and the lazy parse cannot is give up a long match
for a shorter one that leaves the input better placed: a repeat offset two
bytes on, a literal now so that the next match is cheap.

`encode/optimal.rs` is the price model; `Matcher::parse_optimal` is the search.
Both follow libzstd's `btopt` closely in shape: a forward table of the cheapest
known cost to reach each position, filled from every match found along the way
until the paths converge, the 4 096-position table fills, or a match is long
enough to take outright; then a walk back along the cheapest path, which is
committed. Two departures are deliberate:

- **First on hash chains, then a tree.** libzstd's top levels find matches
  with a binary tree that yields every useful length without walking hundreds
  of candidates. The first version walked zalt's existing chain instead,
  keeping each candidate longer than the last — a finder with two milestones
  of tests behind it — and the ablation below found exactly that to be where
  it was slow. The tree came next, and is its own subsection.
- **Prices are allowed to be wrong.** Each table entry carries its own copy of
  the decoder's repeat-offset state, so a repeat match is priced by the path
  that reaches it — two paths to one position can disagree about which offsets
  are cheap, and should. But nothing the parser assumes is *written*. Every
  committed match still goes through `choose_offset_value`, which asks the
  decoder's `resolve_offset`, and through the same sequence writer every level
  shares. A price that is wrong costs bytes. It cannot cost correctness, and
  that property is what made a new parser safe to build in one sitting.

The price model is transcribed from libzstd's `zstd_opt.c`, and the reason
differs from the decoder's: these are heuristics nothing reads back, so a
mature encoder's converged values are a better starting point than new ones.
It is fixed point, because `core` has no logarithm — `log2` as highest set bit
plus a linear fraction, exact at powers of two and within a tenth of a bit
between.

**The lookup tables are derived, not transcribed.** The parser needs a
literals-length and match-length *code* in constant time, hundreds of times per
byte. The small tables are computed at build time by a `const fn` searching the
decoder's own baselines — M5's rule for this module's neighbour — and longer
lengths use libzstd's highest-bit shortcut, which a test then checks against
the decoder's tables **at every legal length**. That test failed on its first
run, at exactly one value: a literals length of 131 072, where the shortcut
computed code 36 — a code that does not exist, and an index past the end of the
price table. 131 071 is the longest literals length the format can express, so
no real sequence reaches it; the clamp is there anyway, because the parser
prices long literal runs one byte at a time and a panic there would be
reachable from content.

Results of the first version, on hash chains, against libzstd's same level:

| level | ZTML documents, size | time | large input, size | time |
|---:|---:|---:|---:|---:|
| 16 | 1.005x | **0.67x** | 1.032x | 1.31x |
| 17 | 1.005x | **0.68x** | 1.030x | 1.50x |
| 18 | 1.003x | 1.45x | 1.025x | 1.87x |
| 19 | **1.002x** | 1.69x | 1.012x | 3.80x |

Level 19 within 0.2% of `zstd -19` on the ZTML documents and 1.2% on large
input, from 1.7% and 64% — but at nearly four times libzstd's time on large
input. On `showcase.zdoc` alone it is 2 882 bytes against `zstd -19`'s 2 885.

##### What level 19's settings were chosen from

| level 19 variant | ZTML documents | large input | time against `-19`, large |
|---|---:|---:|---:|
| depth 128 | 17 008 | 434 038 | 2.4x |
| **depth 256 (chosen)** | 17 010 | 431 361 | 3.8x |
| depth 512 | 17 009 | 429 613 | 5.7x |
| one pass instead of two | +0.15% | +0.03% | about half |
| target length 999 | 0 | −0.08% | — |
| 3-byte matches at repeat offsets | +2 bytes | −21 bytes | — |

- **Search depth is the lever, and a hash chain makes it expensive.** Past 256,
  each further 0.4% costs twice the time. A binary-tree finder is the way to
  have both — built next, below.
- **The scout pass buys little.** libzstd's `btultra2` parses the first block
  once purely to learn its statistics; here that is worth 0.15% on small
  documents for roughly double the time. Kept at 19, the level that promises
  maximum effort, and removed from 18.
- **Three-byte matches were tried and dropped.** libzstd's top levels accept
  them; the version that costs nothing to search for — at repeat offsets,
  which need no hash — moved sizes by noise in both directions. Code that buys
  nothing does not stay.

##### The binary tree, for 16 to 19

libzstd's `bt` finders, in shape (`ZSTD_insertBtAndGetAllMatches`). Each hash
bucket is a binary search tree of earlier positions, ordered by the bytes that
follow them, newest at the root. Inserting a position *is* searching from it: one
descent compares it against a node at a time, hangs each node above or below it,
and steps toward where it belongs — and since the tree is ordered, the nodes it
passes are the ones nearest it in sort order, which is to say the longest
matches. Because searching inserts, a tree serves only the optimal parser; the
lazy parse searches positions it may not then insert, which a tree cannot do.

The results moved the top of the table onto it — level 16 to 19, against
libzstd's same level:

| level | ZTML documents, size | time | large input, size | time |
|---:|---:|---:|---:|---:|
| 16 | 1.004x | 0.77x | **0.990x** | 1.53x |
| 17 | 1.004x | 0.72x | **0.997x** | 1.38x |
| 18 | 1.004x | 0.74x | 1.001x | 1.23x |
| 19 | **1.002x** | 1.44x | **1.003x** | **1.13x** |

**Level 19 went from 3.8x libzstd's time to 1.1x, and from 1.012x its size to
1.003x.** Levels 16 and 17 come out *smaller* than libzstd's on large input.

**Departure from libzstd, on purpose: the tree navigates, it never measures.** A
node's place implies it shares at least `min(common_below, common_above)` bytes
with the position being inserted, and libzstd skips comparing those. So does
zalt — to steer. But a match is *reported* only after a full comparison from
its first byte. A tree made inconsistent by something unforeseen (a slot reused
after the window wraps, an ordering decided under a different limit) would
otherwise report a length the bytes do not support, and the frame would decode
wrong with nothing to say so. Reports happen only on an improvement, a handful
per search, so the check is cheap. It is tested the only way it can be: a test
overwrites every child link with an arbitrary older position — an ordering the
invariants forbid and libzstd would trust — and requires every match reported
afterwards to be real, byte for byte. Without the verification, that test
fails.

**Depth hardly matters to a tree** — 32 to 256 compares measured within 0.05%,
because a descent reaches a leaf long before the budget. **What mattered was how
far it compares** before giving up on ordering two positions. Two that agree all
the way to the limit cannot be ordered, so the tree forgets one of them and
everything below it. At 4 KiB, libzstd's `ZSTD_OPT_NUM`, level 19 was 432 266
bytes on large input; at 64 KiB, **429 181** — text with long duplicated
stretches had been making the tree forget constantly.

A longer compare reopens what the limit guarded against: inserting a long
duplicated region costs the square of its length. libzstd's answer, adopted with
its numbers, is to **skip inserting** positions deep inside a long or
self-overlapping match (`ZSTD_insertBt1`'s return value: up to 192 after a match
past 384 bytes, or to the end of a run whose period is shorter than itself).
That *also* improved the ratio — 429 181 to **427 735** — presumably because
positions deep inside a long match had been crowding the trees with sources
nothing would choose. One thing zalt adds: a skip is clamped so it can never
pass the position the parser will search next. In a tree, searching inserts, so
a skip past that position would insert it *after* newer ones — at a root, in a
tree that must be ordered by age. A test checks the cursor never overshoots.

The quadratic case, checked directly: 2 MB of one 100 KB chunk repeated, a 1 MB
zero run, 2 MB of a 1 000-byte period and ten verbatim copies of RFC 8878 encode
at level 19 at 19.7 MB/s, within 0.2% of `zstd -19`'s size.

The chain–optimal pairing is no longer in the table and is still tested, every
shape through both finders: it is the simpler of the two, and the one a tree bug
would be bisected against.

##### Why this is safe

- **Every level through three decoders.** `tests/encode.rs`'s differential
  matrix gained every level from 1 to 19, plus level 19 under a 1 KiB window
  with small blocks and with a checksum, and an input built for the match
  finder — structured text past a block boundary, with matches at every
  distance — since every other input there is a header boundary, a run or
  noise, none of which gives an optimal parser a real choice to get wrong.
  libzstd, `ruzstd` and zalt read every frame.
- **The parser alone.** Unit tests replay its sequences over every shape the
  lazy parse is tested on, resolve every offset *value* it wrote through the
  decoder's rule, and check that it reaches across blocks and refuses offsets
  past a 1 KiB window.
- **One size assertion, mutation-checked.** Level 19 must be no larger than
  level 15 on structured text. Not a theorem — prices are estimates — but a
  broken price model is invisible to every correctness test, the M5 notes
  record two bugs of exactly that kind, and making literals free (so the
  parser stops matching) fails it.
- **Interop:** 322 checks against the real `zstd` binary, from 112 — and
  since `zalt compress` exists, written by it rather than by the example
  program that stood in for it.
- **Fuzzing:** the `encode_decode` target hard-coded `level: 3`, so it had never
  reached any parser but one. It now takes the level from the fuzzer's input.
  A campaign of 15 minutes on six workers ran **1 116 832** encodes at fuzzer-
  chosen levels under ASan, every frame decoded by libzstd as the oracle and
  then by zalt: no crashes, no disagreements. After the binary tree replaced
  the chain for 16–19, a second campaign of the same size ran **1 083 648**
  more: none either.

**One build-configuration change came with it.** Debug-mode optimal parsing is
some thirty times slower than optimised, and the differential suite went from
about two seconds to fourteen. `zalt-codec` now builds at `opt-level = 1` in
the dev profile; `debug-assertions` and overflow checks are separate keys and
stay on, so every `debug_assert!` still runs. The suite is back to about four
seconds.

##### Encode speed, levels 1 to 19 (2026-10-08)

The baseline, on a quiet machine (`just encode-levels corpus --time`, 61.7 MB,
mostly `varied.txt`), had levels 1–3 at **10–11x libzstd's time** — not the
4.4–5.3x quoted above, which was measured on a 4.5 MB "large" set whose
location was never recorded and is gone. The encoder code had not changed
since; the gap depends on the corpus.

**The profile overturned the explanation above.** It was not per-position
search: `Matcher::insert` was 1.5% and entropy-table builds under 1%. At level
1, `Matcher::parse`'s own loop was 35% (indexing every byte of every match
interior, inlined), the search 18%, sequence coding ~30%, `offset_code_for`
8% — and, once those were visible, **three "build both and keep the smaller"
choices** that the M5 profile, dominated then by the search, had cleared:

1. **libzstd's `fast` and `dfast` for levels 1–4**, from the 1.5.7 source:
   single-slot tables (one on 7/6 bytes; or 8 bytes + 5 bytes), no chain, two
   (`fast`) or four (`dfast`) positions indexed per match, `kSearchStrength`
   stepping, the `ip+1` repeat check and immediate repeats, with `clevels.h`'s
   parameters for input over 256 KB. Offsets are still coded by asking the
   decoder. Sizes come out at libzstd's own: the old chain levels were 7–16%
   *smaller* than `zstd -1..-3`, bought with work libzstd does not do there.
2. **`offset_code_for`'s shortcut**: an offset that is none of the repeats
   (nor `repeats[0] − 1` with no literals) can only be a literal offset, so
   the three trial decodes are skipped; a debug assertion still asks the
   decoder, and a test checks it against the slow path over a grid.
3. **The block splitter's second encode.** It encoded everything with fixed
   strides, then again with the detector's split points wherever it proposed
   any — which on real data is nearly always — keeping the smaller. Measured
   on `corpus/` and on a 38 MB tar of 30 registry crates at levels 5, 9 and 19,
   **the second encode chose the detector's boundaries every time**; encoding
   once with them is byte-identical, at about half the time (level 5:
   5.29x → 2.50x; level 9: 6.97x → 3.32x). The splitter is worth 0.013–0.020%
   on `corpus/` and **~0.2% on the mixed tar** — kept, now at single-encode
   cost. Given up: the guarantee that splitting can never lose to fixed
   strides, which held by construction and now holds by measurement. Not at
   levels 1–4, where the detector alone is ~10% of the time (libzstd does not
   split there either).
4. **Sequence tables chosen by estimate, per table, encoded once.** Every
   block's sequence bitstream was encoded twice — predefined tables and fitted
   ones, all three switched together — and the smaller kept. Now each of LL,
   OF and ML picks its table from libzstd's cost estimate
   (`count × (log − log2 n)` bits, plus the description) and the section is
   encoded once. **Smaller at every level** (−0.002% to −0.042%), because
   choosing per table finds mixes the all-or-nothing comparison could not.

Time against libzstd at the same level, and size, before and after (same
corpus, same machine; swap was higher for the second run, so read the times
as ±10%):

| level | time before | **time now** | size before | **size now** |
|---:|---:|---:|---:|---:|
| 1 | 11.08x | **2.17x** | 0.932x | 0.996x |
| 2 | — | **2.02x** | — | 0.973x |
| 3 | 10.12x | **1.91x** | 0.844x | 0.983x |
| 4 | — | **1.85x** | — | 0.981x |
| 5 | 5.46x | **2.77x** | 0.881x | 0.881x |
| 7 | 4.96x | **2.51x** | 0.957x | 0.956x |
| 9 | 6.88x | **3.34x** | 0.981x | 0.981x |
| 12 | 7.30x | **4.34x** | 0.988x | 0.987x |
| 15 | 2.29x | **1.24x** | 1.021x | 1.021x |
| 17 | 3.09x | **1.45x** | 0.985x | 0.985x |
| 19 | 2.27x | **1.07x** | 1.000x | 0.999x |

What is left at level 1 (206 MB/s in the profiler): the `fast` loop itself
39% (libzstd pipelines the hash and load of neighbouring positions; ours is
serial and bounds-checked), sequence coding ~30%, `memmove` 6%, Huffman 6%.
The worst remaining gap is the lazy chain levels, 9 and 12 at 3.3–4.3x —
libzstd 1.5 searches those with its row-hash finder, not chains.

##### Row-hash finder, levels 5 to 12 (2026-10-09/10)

libzstd 1.5.7 searches its greedy-to-lazy2 levels (5–12) with a row-hash
finder, and moves to binary trees at 13. Each hash bucket is a row of the
16, 32 or 64 most recent positions with a one-byte tag each (8 more hash
bits); a search compares its tag against the whole row at once and fully
compares only matching positions, newest first. No chain to chase.
`Finder::Row` is that, slotted into zalt's existing greedy/lazy loop — repeat
checks, lazy steps and offset coding unchanged — through `find` and `insert`.

**Tag matching is SWAR in safe Rust** (`tag_matches`: XOR with the tag
repeated, an exact zero-byte test, a multiply to gather the flags), tested
against byte-by-byte comparison at every width and on the tags that break
cheaper tricks. **A NEON version belongs in `zalt-neon`** — the user asked for
it to be noted; it is kept to that one function so the backend can replace it.

Sizes on `corpus/` (deterministic; the machine was too loaded to time):

| level | chains | rows, libzstd's parameters | same, no long-match skip | **rows of 64, 64 attempts, no skip** |
|---:|---:|---:|---:|---:|
| 5 | 3 846 022 | +7.1% | +0.26% | **3 625 273 (−5.7%; 0.830x zstd)** |
| 9 | 3 485 299 | +6.8% | +3.0% | **3 449 287 (−1.0%; 0.971x)** |
| 12 | 3 399 360 | +5.0% | +1.1% | **3 436 253 (+1.1%; 0.998x)** |

- **libzstd's long-match rule is not taken.** Its row finder indexes only the
  first 96 and last 32 positions of a match over 384 bytes; here that cost
  5–7% of size, because repetitive input lives on exactly those candidates.
- **Depth matters more than libzstd's row sizes suggest.** "Attempts" count
  only tag matches, so a row of 64 with 64 attempts is mostly 8 SWAR steps per
  search — likely cheap, but not yet timed.
**The sweep, timed** (2026-10-10, language servers suspended, load ~2,
swap 79%; time as a multiple of libzstd's at the same level, size against the
chains these replaced):

| rows / attempts | L5 time | L5 size | L9 time | L9 size | L12 time | L12 size |
|---|---:|---:|---:|---:|---:|---:|
| chains | 2.82x | 3 846 022 | 3.33x | 3 485 299 | 4.27x | 3 399 360 |
| 16 / 8 | 2.75x | +0.3% | 1.50x | +5.9% | 0.92x | +8.4% |
| 16 / 16 | 2.93x | −2.3% | 1.62x | +3.0% | 0.98x | +5.3% |
| 32 / 32 | 3.27x | −4.4% | 1.85x | +0.6% | 1.13x | +2.8% |
| 64 / 32 | 3.56x | −4.3% | 2.00x | +0.8% | 1.21x | +3.1% |
| 64 / 64 | 3.97x | −5.7% | 2.33x | −1.0% | 1.42x | +1.1% |

At level 5 rows are not faster than depth-12 chains — they buy size; at 9 and
12 they win outright. 64 rows with 32 attempts lost to 32 with 32 on both
axes. **The table: 16/16 at 5, 32/32 at 6–7, 64/64 from 8 to 12.** Levels
13–15 stay on chains for now (libzstd's `btlazy2` is the model; tried
below).

Every level after the fast-level work, the splitter and table-estimate
changes, and the rows (same conditions):

| level | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 15 | 17 | 19 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| time vs zstd | 2.16x | 1.98x | 1.89x | 1.86x | 2.89x | 2.45x | 2.26x | 2.25x | 2.35x | 1.89x | 1.42x | 1.43x | 1.75x | 1.34x | 1.46x | 1.07x |
| size vs zstd | 0.996x | 0.973x | 0.983x | 0.981x | 0.861x | 0.927x | 0.940x | 0.945x | 0.971x | 0.988x | 0.999x | 0.998x | 0.983x | 1.021x | 0.985x | 0.999x |

Two days earlier the same corpus put level 1 at 11.08x, 5 at 5.46x, 9 at
6.88x and 12 at 7.30x.

##### Lazy parse over the binary tree, levels 13 to 15 (2026-10-11) — not adopted

libzstd uses `btlazy2` (a binary tree with lazy parsing) at 13–15, with 16,
32 and 64 compares (searchLog 4/5/6 in `clevels.h`). zalt's tree had served
only the optimal parse, because in a tree searching *is* inserting and the
lazy loop searches some positions and indexes others afterwards. It now serves
both: the lazy loop searches through `find_tree`, which goes through the same
in-order cursor the optimal parser uses, so every position is inserted exactly
once and in order. A test holds the tree's lazy parse within 5% of a 4096-deep
chain on structured text (measured 1.3%); with lookahead positions inserted
twice it came out 30% worse, and failed — a corrupt tree replays correctly,
so only cost can show it.

**Sizes** (`corpus/`, vs zstd at the same level) saturate by depth 64:

| tree depth | 16 | 32 | 64 | 128 | 256 | 512 |
|---|---:|---:|---:|---:|---:|---:|
| L13 | 0.992x | 0.980x | 0.979x | 0.978x | 0.978x | 0.979x |
| L15 | 1.040x | 1.028x | 1.026x | 1.026x | 1.026x | 1.026x |

zalt's tree spans the whole window at every level. libzstd's `btlazy2` tree
holds `2^chainLog / 2` positions — 2M at level 13 in a 4 MiB window — so its
13 searches half the window and its 15 all of it. That is why the tree beats
`zstd -13` by 2% and gains nothing at 15.

**Time**, two interleaved rounds (swap 66%, load about 2.2, language servers
suspended; the rounds agree within 4%), round 1 shown:

| finder | L13 time | L13 size | L14 time | L14 size | L15 time | L15 size |
|---|---:|---:|---:|---:|---:|---:|
| chains 192/256/384 | 1.79x | 0.983x | 1.54x | 0.994x | 1.36x | 1.021x |
| tree, depth 16 | 2.30x | 0.992x | 1.60x | 1.008x | 1.00x | 1.040x |
| tree, depth 32 | 2.50x | 0.980x | 1.71x | 0.997x | 1.07x | 1.028x |
| tree, depth 64 | 2.50x | 0.979x | 1.75x | 0.995x | 1.07x | 1.026x |

The tree runs about 10 MB/s whatever its depth, against 13.6 MB/s for the
192-deep chain: the cost is not the search but the insertion. zalt's tree
sorts every position as it is inserted, match interiors included, where a
chain only links it. **13–15 stay on chains.** Only at 15 does the tree win on
time, for 0.5% of size (1.07x at 1.026x against 1.36x at 1.021x) — left open.

What libzstd does instead is a *deferred* tree (`ZSTD_updateDUBT`,
`ZSTD_DUBT_findBestMatch` in `zstd_lazy.c`): a position joins its bucket
unsorted, as cheaply as a chain link, and is sorted into the tree only when a
later search passes through it. Interiors no search reaches are never sorted.
That is the finder 13–15 would need to beat their chains.

##### The deferred tree, levels 13 to 15 (2026-10-11/12) — not adopted either

Built after the above: under the lazy parse the tree now defers, as
`btlazy2` does. `add_unsorted` links a position into its bucket with the
second slot marked `UNSORTED` (`u32::MAX − 1`, which no position can equal,
where libzstd's mark of 1 can); `find_tree` walks the bucket's unsorted
positions newest first up to the depth, cuts the list if it is still going,
sorts the walked positions oldest first with the existing descent
(`tree_descend`, split out of `tree_update`), then searches. The optimal
parse keeps sorting on insert — it searches every position, so nothing would
be deferred. Tests: the deferred tree within 0.5% of the same tree sorted on
insert (measured 0.08%), which fails with the walk unsorted (3.7%) or sorted
newest first (1.2%); a 1 KiB-window run where every slot is reused, for
replay, offsets and panics.

**Size** depends on depth now, because a shallow walk cuts the list and drops
the tree behind it: level 13 at 1.029x `zstd -13` (depth 16), 1.000x (32),
0.983x (64), 0.975x (128).

**Time is worse**, and grows with depth (two rounds, agreeing within 4%):

| finder | L13 time | L13 size | speed |
|---|---:|---:|---:|
| chains, 192 | 1.69–1.83x | 0.983x | 13–15 MB/s |
| tree sorted on insert, any depth | 2.3–2.5x | 0.979x | ~10 MB/s |
| deferred, 32 | 2.84x | 1.000x | 8.6 MB/s |
| deferred, 64 | 4.32–4.48x | 0.983x | 5.6 MB/s |
| deferred, 128 | 6.60x | 0.975x | 3.7 MB/s |

A profile (`sample`, level 13, depth 64) put 71% of the time in
`common_prefix`, nearly all of it from sorting walked positions — the final
search was about 3%. The lazy parse searches almost every literal position,
so little is deferred, and what is sorted compares long: runs and repeated
lines agree for kilobytes. With the NEON `common_prefix` below the deferred
tree ran about 1.5x faster (depth 64: 3.0–3.1x; 128: 4.2x) and still lost to
the chains. **13–15 stay on chains**; the deferred tree is kept, tested and
unused, as the place to start if a cheaper sort turns up.

##### The encoder's kernel seam, and `common_prefix` in NEON (2026-10-12)

`common_prefix` — how far a candidate agrees with the current position — is
now a [`Backend`] method, the encoder's one kernel, so it can be accelerated
the way the decoder's copies are. `Matcher` is generic over the backend
(default `Scalar`), `compress_with::<B>` picks one, and `zalt_embed::compress`
picks the build's, as `decompress` does; the CLI, the archive writer and the
bench go through it. `zalt_codec::encode::compress` stays scalar.

The NEON kernel is a hybrid. Measured per call (M1, one process, variants
interleaved, best of nine), as multiples of the scalar 8-byte loop:

| agreement | 3 | 7 | 12 | 24 | 64 | 300 | 4000 |
|---|---:|---:|---:|---:|---:|---:|---:|
| safe 16-byte (`u128`) | 1.20x | 1.26x | 0.96x | 0.84x | 0.78x | 0.76x | 0.65x |
| safe 32-byte unrolled | 1.12x | 1.17x | 1.03x | 0.80x | 0.62x | 0.93x | 0.50x |
| NEON from byte 0 | 1.06x | 1.11x | 0.84x | 0.67x | 0.58x | 0.69x | 0.39x |
| **scalar first word, then NEON** | 0.99x | 1.00x | 0.96x | 0.76x | 0.57x | 0.74x | 0.39x |

Most calls end in the first word — candidates sharing a hash and little
else — where a vector compare and its mask cost more than one XOR; past it
the vector loop wins outright. The mask is `vshrn_n_u16::<4>` over the
inverted `vceqq_u8`, four bits per byte (aarch64 has no `movemask`).

Frames are byte-identical whichever backend measures: `zalt-neon`'s tests
encode every level both ways, and `backend_equivalence` now fuzzes the kernel
and whole encodes (16 KiB cap; coverage 1,162 → 3,831 at 127 → 82 runs/s).
End to end against zstd, two rounds (swap 84%, load ~2):

| level | 1 | 5 | 9 | 12 | 19 |
|---|---:|---:|---:|---:|---:|
| scalar | 2.39x | 2.85x | 2.41–2.44x | 1.45x | 1.07x |
| NEON | **2.12x** | 2.82–2.84x | 2.32–2.36x | 1.41–1.45x | 1.10x |

Level 1 gains 11%; 5–12 gain 1–4%, their comparisons being mostly short;
level 19 is 3% *slower* in both rounds, unexplained — inlining is the first
suspect. 13 and 15 swung 20–25% between rounds for both builds and are not
reported.

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

### M7 — ZSTA archives: portable R1

`doc/zstd-archives_new.md` (draft 0.2) specifies **ZSTA**: a multi-file archive
built entirely from standard zstd frames, at once a valid `.tar.zst` —
`zstd -dc a.tar.zst | tar -x` extracts it — and randomly accessible, through a
trailing seek table and a Central Index that list the contents and locate any
member without decoding anything else. `crates/zalt-archive` implements the
first milestone: the **portable profile** and an **R1 reader** (spec §14.2).

- [x] Format core: VarInt (§2), the Archive Header (§4), the seek table in its
      §8.2 profile, and the Central Index (§7), each with a strict parser.
- [x] Writer: pax-over-ustar tar (§9.1), §12's frame policy — solid groups,
      single frames, chunked large files — and frames compressed in parallel.
- [x] R1 reader: indexed open with every §11.1 check, listing, extracting one
      member or all of them, frames decoded in parallel, and `verify`.
- [x] Extraction under §13: paths, links, special files, limits.
- [x] `zalt archive create | list | extract | verify`.
- [x] The probe imported as the oracle; three fuzz targets (§14.3).
- [x] macOS metadata (§9.3), entry hashes (§7.3), append and recovery (R2,
      §10–§11.3), FIFOs and device nodes.
- [ ] Dictionaries (§6).

#### Where it sits

Above the codec, and the only crate that knows the ZSTA layout exists.
`zalt-codec` writes and reads frames and has never heard of tar; the dependency
rule of §1 holds in the usual direction. The crate is `forbid(unsafe_code)`,
which is exactly why §9.3's macOS metadata is not in this milestone — extended
attributes, ACLs and BSD flags all need system calls `std` does not expose.

#### Written from the spec, judged by others

**The index and tar code were written from the spec without reading the
probe's.** The probe then stands as an independent second reading of §7 — the
role `ruzstd` plays for the codec — so a misreading would have to be made twice,
independently, to pass.

The spec defines conformance by what *other* software accepts (§14.1), so the
tests do too. Every archive the writer produces is judged by three things that
are not `zalt-archive`:

| judge | what it checks | result |
|---|---|---|
| libzstd's reference seekable reader | the same frame map from the same table; the whole tar stream; every frame; 200 random ranges | agrees, at levels 3 and 19 and empty |
| the probe's §7 decoder | every Central Index field | agrees |
| `zstd -t`, `zstd -dc \| tar -t`, `tar -x` | stock-tool compatibility | GNU tar lists and extracts identically, NFD path included |

And before any of that, the probe's own suite — written for libzstd as the
encoder — passes **16/16 with zalt as the encoder** at levels 1, 3, 9, 16 and
19, including §5.2's frame conformance at every level it checks. zalt was a
conformant ZSTA encoder before a line of `zalt-archive` existed.

#### Safety, and the tests that can fail

Two defenses against writing outside the destination, each sufficient alone:
**symlinks are created last**, so nothing this extraction makes can redirect a
later write; and **every parent directory is checked to be a real directory**
before anything is created in it, which also covers a symlink that was already
there. Each was mutation-checked: removing the parent check fails the
pre-existing-symlink test; removing *both* lets the planted-symlink test's write
escape the root and fail it. Neither test is a formality.

Other properties checked by breaking them: a member present in the tar stream
but **hidden from the index** opens and lists cleanly — the index is
self-consistent — and `verify` catches it, because the members no longer tile
the stream; an index that lies about a member's path is caught when the member
is read, since the tar header is compared before content is returned (§7.5);
flipping one byte anywhere in an index fails its checksum; a corrupted data
frame fails the entries in it and no others.

What `std` cannot give is stated in the module rather than glossed: checks are
made by path and acted on by path, so a process writing to the destination
*during* extraction could race them. §13 names the fix — `openat`-style,
descriptor-relative creation — and `std` does not expose it. A fresh, private
destination closes the race in practice.

**Output does not depend on the thread count.** Frames are compressed
independently and written in order, so one thread and eight produce identical
bytes — §12.4's reproducibility at any parallelism, asserted by a test.

#### What ZSTA found in the codec

**zalt could not decode a stream that ends in a skippable frame.** Every ZSTA
archive ends in two — the Central Index and the seek table — and so does any
file carrying zstd's seekable-format seek table; `zstd(1)` reads them.

After a skippable frame the decoder steps over it and waits for the next magic
number, reporting `NeedMoreInput`, which is right mid-stream. But the one-shot
decoder, the embed layer, the CLI *and the streaming tests' own helper* all
judged the end of input by the last status returned, so all four reported
these streams as truncated. No test noticed because **every skippable-frame
test put the skippable frame before a real one**; none put it last.

The decoder now answers the question directly — `is_between_frames()`: between
frames with nothing buffered — and all four use it. The new test was checked by
putting the old rule back, under which it fails; a stream cut inside a trailing
skippable frame, a partial magic number after a frame, and empty input are all
still refused. A stream of only skippable frames now decodes to nothing, as
libzstd's `ZSTD_decompress` does.

**Left open:** the C ABI reports status per call and has no way to say "between
frames", so a C host still cannot tell a stream that ended on a skippable frame
from a truncated one. Fixing that is an ABI addition, and is recorded here
rather than made unasked.

#### Fuzzing

| target | property | runs | result |
|---|---|---:|---|
| `archive_open` | arbitrary bytes opened, verified and read: no panic, hang or unbounded allocation | 34.1 M | clean |
| `archive_index` | the fuzzer's bytes as the *decoded* index body, wrapped and signed as a writer would — past the checksum — and anything accepted re-encodes to itself | 3.7 M | clean |
| `archive_varint` | anything accepted is the one canonical spelling of its value | 258 M | clean |

`archive_open` was seeded with three real archives and grew from 1 124 coverage
edges to 1 761; `archive_index` from 19 to 1 982.

#### Findings for the spec

Recorded here for the next draft; `doc/zstd-archives_new.md` is unchanged.

- **§7.2 does not say XXH3-128's byte order.** "Raw digest" fits BLAKE3 and
  SHA-256, whose digests are bytes, but XXH3-128 is a 128-bit integer. zalt
  writes xxHash's canonical form (`XXH128_canonicalFromHash`: the high 64 bits
  then the low, each big-endian, as `xxhsum -H2` prints); the spec should name
  it, or two readers can each be right and disagree.
- **Q6 is answered for zalt: BLAKE3 by default** (see *Entry hashes*). The
  spec could say so, or at least that writers SHOULD prefer a cryptographic
  hash where Q3 or Q4 may later apply.
- **A recovered archive cannot know its original `Hash_Algorithm`.** Only the
  Central Index records it, and §11.3's recovery exists for when the index is
  gone. zalt re-hashes with its own default. If the algorithm should survive,
  the Archive Header could carry it as a TLV.
- **§11.2 step 6 checks the hash after the content is written.** A streaming
  reader cannot do otherwise; the spec could add that a member failing its
  hash MUST NOT be left in place, as zalt's extraction now ensures.

- **macOS's `tar` is bsdtar, and it lists NFD paths in NFC.** The probe's
  `stock_cli_zstd_and_tar` fails under it *with libzstd as the encoder*: every
  path matches except `données/résumé.txt`, decomposed in the archive, listed
  precomposed. The archive is right — §9.3 stores paths as given — but §14.1
  requires listings to pass "with bsdtar", so either it needs a
  normalization-insensitive comparison for bsdtar or the requirement needs a
  note. A second bsdtar difference is *not* a ZSTA property, and is recorded so
  it is not mistaken for one: `--use-compress-program zstd` fails on **any**
  `.tar.zst` under bsdtar, a plain `gtar | zstd` one included, because GNU tar
  appends `-d` when reading through the program and bsdtar does not. The
  probe's test used GNU's spelling; it now passes `zstd -d` to bsdtar, and
  prefers GNU tar where it is installed.
- **§5.2's 2^27 window ceiling is above zalt's untrusted decode ceiling of
  2^23.** A reader must opt into 2^27 to read conformant archives written by
  `zstd --ultra -22`, and with parallel decode that is 128 MiB per frame in
  flight. `zalt-archive` reads to 2^27, deliberately.
- **`Frame_Content_Size` is present in zalt's frames only by coupling.** zalt's
  writer omits it for some frames under 256 bytes; ZSTA's data frames are
  always at least 512 and its index bodies single-segment, so it holds — and
  the archive writer now checks it on every frame rather than relying on that.
- **`Path_Shared` need not be maximal.** §7.1 does not say a writer must share
  the longest prefix, so two byte-different indexes can describe the same
  members — the ambiguity §2.1 forbids for VarInt. Worth a MUST, or a note.
- **End of archive: §11.3 and §5.4 need one rule for where the tar stream
  ends.** §11.3 says recovery should "treat the last data frame that decodes
  to only zero bytes as the Terminator". A large all-zero file split across
  frames (§5.3) has middle chunks that also decode to only zeros, so taken
  literally the rule cuts such a file in half whenever the real Terminator is
  missing — the very case recovery exists for. What actually ends a tar
  stream is two zero blocks *where a member header is expected*, and §7.5
  already makes the tar stream authoritative. Suggested wording for §11.3
  step 4: *"The Terminator is the first data frame that begins at a member
  boundary and decodes to only zero bytes. If there is none, the archive was
  cut short: it ends at the last frame boundary that is also a member
  boundary."* Two related gaps: §5.4 says the Terminator holds no entry data
  but not that it must *start* at a member boundary, which the rule above
  relies on and every conformant writer satisfies; and nothing says what a
  reader does with end-of-archive blocks found *inside* a data frame rather
  than in a Terminator of their own (zalt's recovery treats them as the end of
  what it can trust). `zalt-archive` implements the suggested rule; the test
  `an_all_zero_chunk_is_not_mistaken_for_the_terminator` fails under the
  literal one.
- **Frame boundaries cost ratio exactly where files repeat each other.**
  Archiving `crates/` at level 19: **949 567 bytes against `tar | zstd -19`'s
  615 264, 54% larger** — and zalt's encoder is not why: it matches `zstd -19`
  on every frame (1.00x) and on the whole stream as one frame. The difference
  is two test fixtures, a 300 KB random file and its zstd-compressed form,
  which stores the random bytes nearly verbatim. In one stream the second
  matches the first; §12.1 gives every file over 256 KiB its own frame, so it
  cannot. That is the price of random access, and this tree is its worst case.
  §12.2's grouping advice could say so; a policy that lets medium files join
  solid groups trades a larger decode per extraction for exactly this.

  **Measured and decided (2026-10-05): the writer's defaults are now 8 MiB /
  8 MiB** — every entry under 8 MiB may join a solid group, and groups close
  at 8 MiB — instead of §12.1's 256 KiB / 4 MiB. Sizes against zstd on the
  *identical* tar stream in one frame (so only frame boundaries differ), and
  the bytes a one-file extraction decodes:

  | policy (threshold / group) | `crates/` L19 | registry 146 MiB L19 | registry L3 | mean / worst decode (registry) |
  |---|---:|---:|---:|---|
  | 256K / 4M (§12.1) | 1.515x | 1.323x | 1.019x | 3.6 / 4 MiB |
  | 4M / 4M | 1.009x | 1.308x | 1.015x | 3.9 / 4 MiB |
  | 256K / 8M | 1.515x | 1.248x | 0.982x | 6.6 / 8 MiB |
  | **8M / 8M (new default)** | **1.009x** | **1.215x** | **0.975x** | 7.9 / 8 MiB |
  | 4M / 16M | 1.009x | 1.124x | 0.959x | 15.6 / 16 MiB |
  | 4M / 32M | 1.009x | 1.065x | 0.950x | 30.4 / 32 MiB |
  | one frame (zalt's floor) | 1.009x | 1.030x | 0.929x | everything |

  Two separate effects. Letting medium files join removes the near-duplicate
  case outright (`crates/`). Group size governs broadly redundant trees, with
  **no knee at the 8 MiB match window**: each doubling roughly halves the
  overhead above the floor and doubles the decode per extraction, because
  every frame starts cold. 8 MiB is a middle setting chosen for now, to be
  revisited with timing. At level 3 zalt already writes archives smaller than
  `tar | zstd -3` here, partly because its level 3 out-compresses zstd's by ~7%
  on this data. End to end, `zalt a c -19` on `crates/` went from 1.515x to
  **1.007x** of `zstd -19` on the same stream.

  Not measured: extraction *time* (swap was at 92%; bytes decoded stand in),
  and a typical macOS corpus — the registry holds several versions of each
  crate side by side, which flatters bigger groups. For the spec: §12.1's
  256 KiB threshold costs more than it buys whenever files near each other
  resemble each other.

#### R2: append (§10) and recovery (§11.3)

`zalt-archive` is now an R2 reader. Both operations end the same way — a
known-good prefix of the file, then a new Terminator, Central Index and seek
table — so both are one mechanism: a writer *resumed* from existing state
(`Writer::resume`, crate-private). `append` builds that state from the index;
`recover` builds it from a walk of the file.

- **Append** opens by the index, checks the Terminator is whole zero blocks
  (§5.4), cuts the file there and resumes. Old records and every byte before
  the old Terminator are unchanged (tested). The Archive Header is never
  rewritten. A seek table without checksums is refused, since its old frames
  would be listed with checksum 0. `zalt archive append` works on a copy in
  the same directory and renames it into place, as §10 recommends; a refused
  append leaves the archive byte-identical (tested).
- **Recovery** walks frames from offset 0 with a new codec function,
  `zalt_codec::frame::compressed_size` — the frame's length from its headers
  and block headers, libzstd's `ZSTD_findFrameCompressedSize`, which zalt did
  not have — decodes each data frame, and parses the tar stream across them.
  It keeps the longest prefix that is a valid archive body, copies it byte for
  byte, and writes a new trailer. The damaged file is only read.
- **Duplicate paths are now refused by the writer**, new archives and appends
  alike, before anything is written. §10 rules out replacing a member, and an
  index looked up by path cannot hold two. Tar's own rule is "last one
  wins"; **decided 2026-10-06: keep refusing for now.**

**Spec finding — §11.3's Terminator rule** (also in *Findings for the spec*
above). "Treat the last data frame that
decodes to only zero bytes as the Terminator" also matches a middle chunk of
a large all-zero file, so recovering an archive that lost its Terminator by
that rule cuts that file in half. zalt follows the tar stream instead, which
§7.5 makes authoritative: the archive ends at a frame that starts at a member
boundary and is all zero blocks, and with no such frame, at the last frame
boundary that is also a member boundary. The two rules agree whenever a real
Terminator exists. Suggested wording: "the first data frame that begins at a
member boundary and decodes to only zero bytes".

**Bug found by the oracle, from the metadata round.** The reader compared the
index's optional metadata (names, birth time, flags, devices, xattrs) with the
tar headers strictly, so an index that *omits* one — as the probe's writer
does for owner names — failed every member. §7.2 makes them optional and §7.5
makes the index a cache: now only a field the index has must match. A
regression test pins both halves.

Evidence:

- `tests/r2.rs` (12): append keeps old bytes and records; append to an empty
  archive, twice, at another level; recovery at every frame boundary ±9 bytes
  and a 61-byte stride yields a verifying archive whose members are a prefix
  of the original's, never shrinking as more survives; damaged index and
  footer rebuilt whole; a corrupt data frame keeps exactly the members before
  it; a crash anywhere in an append keeps every old member; the all-zero
  chunk case. Two mutants — §11.3's literal rule, and cutting without regard
  to member boundaries — each fail tests.
- `tests/oracle.rs`: appended and recovered archives match libzstd's
  reference seekable reader frame by frame and the probe's independent §7
  decoder; a **libzstd-written** archive with vendor frames between its data
  frames (Q7) is recovered and appended to; `compressed_size` equals
  `ZSTD_findFrameCompressedSize` on every frame of four archives, and both
  refuse every cut tried.
- Fuzz `archive_recover`: arbitrary bytes, and whenever recovery succeeds
  the result must open and verify. 81 k runs from three seeds, clean.

**Salvage past damage (2026-10-06).** Recovery no longer stops at the first
damaged frame. Two passes: frame extents (from the seek table if it
survived — it gives a damaged frame's length even when the frame's own block
headers are gone — else by walking headers), then decode and tar walk. A
damaged frame's share of the tar stream is still known (seek table, or its
`Frame_Content_Size`), so:

- damage inside one member's content costs only that member — its header,
  read before the damage, says where the next begins;
- damage that took member headers loses the thread; a surviving Central
  Index picks it up again at the next member it says starts at a frame
  boundary, once the tar header there names that member. Without an index
  the walk stops — see the open question below.

Output is the intact *runs* — stretches between member boundaries that are
also frame boundaries; with zalt's writer one solid group, one file, or one
chunked file — written through byte for byte, never re-encoded, offsets
shifted. A run is kept only if all its frames decode and its members tile it
exactly. `Recovered` names the members lost (from the index when there is one,
since the walk cannot name members whose headers it never read).

Tested in `tests/r2.rs` (16): a flipped byte at every 7th position of every
data frame, trailer intact — every member the damaged frame does not touch
comes back byte for byte, those after it included, and exactly the touched
ones are reported lost; a reserved block type making a frame's length
unreadable, bridged by the seek table; damage inside a file with no trailer
costs that file alone; lost headers stop the walk without an index and resync
with one; and a **libzstd-written** archive whose frames decode but hold a bad
tar header (damage before compression), built with the probe since our writer
cannot write one — its run is dropped whole. Mutants: no gap bridging (1
test fails), no index resync (3), no tiling check (1, and only after the test
was moved to the second member of a frame — the first version put the bad
header first, where an emptiness check already caught it, and the mutant
survived). Fuzz `archive_recover`: 147 k more runs, clean, now also asserting
no member is both kept and lost.

**Search, opt-in (decided 2026-10-06).** With headers lost and *no* index,
`RecoverOptions::search` (`zalt archive recover --search`) resumes at the next
frame that *starts* with a valid tar header. It is a guess an archived `.tar`
can fool — its content is full of valid headers — so every member found that
way is listed in `Recovered::found_by_search` and printed in full. Two tests
pin both sides: search recovers what the index-less walk cannot, and when it
is fooled by an inner tar the invented members are all in `found_by_search`
(and never appear without `--search`). Disabling the search fails both.

#### Large archives (2026-10-06)

Measured deterministically, since swap stood at 93% and timing would mean
nothing: `Archive::bytes_decoded()` counts content decoded, so what an
operation costs is a number a test can assert.

- **Reading decoded each solid group once per member, twice when
  extracting.** `verify` checked every member's headers by decoding the frame
  holding them; extraction did that and decoded it again for the content. On
  400 small files in one group: **`verify` decoded 400x the tar stream,
  reading every member 799x** — members × group size, which on a real tree is
  hundreds of times the archive. A 64 MiB LRU of decoded frames, shared by an
  open `Archive` and handed out as `Arc`s, brings both to **1.0x** (cold,
  each on a fresh open; regression test in `tests/roundtrip.rs`). The
  filesystem extractor read directories' and symlinks' headers again in its
  final passes, after their frames had left the cache; it now reads them in
  the main pass and keeps the parsed headers (≤1.5x test in `tests/fs.rs`).
- Recovery's index lookup per frame was a linear scan and its lost-list
  de-duplication quadratic; now a binary search and a set.
- **Members past 4 GiB and 8 GiB** round-trip — 5 GiB (165 frames) and 9 GiB
  (293 frames, with the pax `size` record §9.1 requires past the octal field),
  built in memory from zeros and checked through a counting sink: 382 MB peak
  RSS, bounded by the decode batch and the cache, not the archive.
  `just archive-huge`; ignored by `just test`.
- **Creating is bounded by the encoder, not the archive layer.** On `crates/`
  (2.4 MB of tar), `zalt archive create` takes ~8 ms more than `zalt c -3` on
  the same stream, and `zalt c -3` ran at 0.15x `zstd -3 -T1` (interleaved,
  best of 7; noisy machine) — consistent with the 4–6x gap at levels 1–3
  recorded above. Metadata capture costs nothing measurable (87–92 ms with
  every combination of `--no-acls/--no-xattrs/--no-fflags`). Closing the
  low-level encoder gap is the lever for large archives.
- `just archive-bench [CORPUS] [ROUNDS]` (`ci/archive-bench.py`): create,
  verify, list, extract all, extract one against `gtar | zstd -T0`,
  interleaved, best-of, as ratios; append and recover against zalt's own
  verify. Refuses above 75% swap and suspends the language servers itself.

**First quiet-machine run (2026-10-08), and three fixes it led to.** On a
578 MB slice of the cargo registry (68 MB archive, 12.4% smaller than
`gtar | zstd -3`), best of 3, zalt against the stock pipeline doing the same
job (ratio above 1 is zalt faster):

| operation | first run | after the fixes |
|---|---:|---:|
| create | 0.63x | 0.62x |
| verify (against `zstd -t`) | 0.30x | **0.93x** |
| list | 7.22x | 7.01x |
| extract all | 0.58x | **0.72x** |
| extract one | 10.29x | 10.18x |
| recover (zalt alone) | 3.96 s | **0.37 s** |

- **`verify` decoded everything twice once an archive outgrew the cache** —
  a parallel pass over the frames, then a single-threaded one to check
  headers, because the two checks were written separately and the test
  archive fit in the cache, which hid the second pass. Now one pass: a
  window of frames decoded in parallel (`Archive::prefetch`), then the
  headers of the members it completes. `ReadLimits::cache_bytes` makes the
  cache size settable, and the regression test now uses an archive forty
  times its cache: 1.00x, against 2.00x for the old shape.
- **Extraction decoded one frame at a time**; extracting everything now
  decodes a window ahead in parallel. (A selection does not, so frames between
  chosen members are not decoded for nothing.)
- **Recovery's tar walk removed each member from the front of a frame-sized
  buffer** — a shift of the rest of the frame per member, quadratic within a
  solid group of thousands of files, and 90% of recovery's time. It now
  parses members in place and carries over only a header split across
  frames. Its decoding is also batched in parallel.

What is left in `create` (0.62x) is the encoder's low-level speed — the
fast/dfast work next — and in `extract all` (0.72x) the decoder's per-core
gap to libzstd plus the filesystem work per file.

#### macOS metadata and descriptor-relative extraction (`native-fs`)

The `fs` module now sits behind the `native-fs` feature (zalt-cli turns it
on). Without it, the format, writer, reader and the metadata text forms
(`meta`) build with no new dependencies — the fuzz targets use it that way, and
`just clippy` checks both.

| dependency | for | instead of |
|---|---|---|
| `cap-std`, `cap-fs-ext` 4 | every extraction step relative to an open directory: `O_NOFOLLOW` per component, `O_EXCL`, `symlinkat`, `linkat`, `renameat` | path checks followed by path actions |
| `rustix` 1 (already in the tree under cap-std) | xattrs through the descriptor | the `xattr` crate — see below |
| `nix` 0.31 (`fs`, `user`) | owner names, `fchownat`/`fchmodat`, `chflags`, `mkfifo`/`mknod` | hand-written FFI |
| `exacl` 0.13 (macOS) | NFSv4 ACLs | hand-written FFI |

What is archived and restored: xattrs (resource forks included,
`com.apple.decmpfs` never, `com.apple.provenance` only with
`--keep-provenance`), ACLs as `SCHILY.acl.ace`, BSD flags as `SCHILY.fflags`
(`UF_COMPRESSED` and system-managed bits never), birth time as
`LIBARCHIVE.creationtime`, uname/gname, FIFOs, and device nodes (extraction
only with `--devices`). A quarantined archive passes its quarantine on (§9.3).
Every spelling was taken from bsdtar 3.7.4's output, and `tests/oracle.rs`
checks both directions: bsdtar restores what zalt archives, and zalt reads
what bsdtar archives, both xattr forms included.

**Extraction no longer has the race the M7 notes above conceded.** Every
directory is opened from its parent's descriptor with `O_NOFOLLOW`, every entry
created by a single name in it, and metadata applied through the file's own
descriptor. Flags and ACLs have only path-based calls on macOS; they go
through `/dev/fd/N`, which the kernel resolves to the descriptor's vnode — a
file renamed away from its name still receives them, and a unit test pins it.
The one path-based step left is creating a FIFO or device node (macOS has
`mkfifoat`/`mknodat`, but neither nix nor rustix exposes them there): the node
is made under an unguessable name, found through the parent's descriptor or
refused, and moved into place with `renameat`. That path for device nodes
needs root, so the test suite covers it only through FIFOs; it was checked by
hand (2026-10-04): `/dev/null` and `/dev/zero` archived from the live system,
extracted with `sudo zalt archive extract --devices`, came back as character
devices 3,2 and 3,3 that behave as the originals.

The race test swaps a directory for a symlink with `renameatx_np(RENAME_SWAP)`
— atomic, so the directory is never missing — holding each state 30 µs. A
path-based control extractor under the same swapper **escapes within 1–5
attempts**; the cap-std extractor, 30 attempts under ~100 swaps, **never**.
The first swapper used two renames and was too weak to break even the control
on a quiet machine (the directory was *missing* in 70% of the control's
checks); the control is what caught that.

**The `xattr` crate truncates resource forks to 4 KiB on macOS.** Its reader
tries a 4 KiB buffer first and assumes a value that does not fit fails with
`ERANGE`. Ordinary xattrs do; `com.apple.ResourceFork` is read like a file and
*succeeds short*. A 5 MB fork archived through it came back as 4 096 bytes,
with no error. zalt reads every value as size query → read → size query again,
retrying on change and failing on a short read of an unchanged size, and caps
one entry's xattrs at 48 MiB so the writer never produces a header its own
reader (64 MiB) refuses. rustix was then checked call by call, in source and on
this machine: NULL for an empty get buffer (macOS returns `ERANGE` otherwise),
size-0 list queries, `XATTR_NOFOLLOW` on the `l*` calls, `ENOATTR`, exact 4096
and 4097-byte values, and forks of 5, 80 and 300 MiB read whole in one call.
No defect found. The round-trip test compares forks through
`..namedfork/rsrc`, which goes through neither implementation, and fails
against a reintroduced 4 KiB read.

A new fuzz target, `archive_meta`, feeds the ACL, flag, base64 and `%XX`
parsers directly — random bytes rarely get a tar header past its checksum:
2.5 M runs clean, 77 of its 821 corpus entries ACL-shaped.

#### Entry hashes (§7.3, 2026-10-12/14)

Every member with content now carries a hash of it, written by the writer,
checked whenever the member is read, by `verify`, and on extraction.

**BLAKE3 is the default — the user's decision on Q6.** A non-cryptographic
hash catches accidental damage as well as any (a random corruption slips past
128 bits with odds of about 2⁻¹²⁸), but anyone choosing the input can collide
it. That breaks the two uses Q3 and Q4 propose: deduplicating by hash, and a
signed index whose signature covers the hashes. A default sticks, and an
archive hashed with XXH3 would need re-hashing before either could apply.
Without a signature a cryptographic hash protects nothing extra — whoever can
rewrite content can rewrite its digest — so XXH3-128 stays available
(`--hash xxh3`) for archives that only need to detect accidents, and `none`
for none.

**Where the code comes from.**

- **BLAKE3: the `blake3` crate**, the BLAKE3 authors' own, with `pure`. Read
  before adding (1.8.7): by default it compiles its NEON path from C on
  aarch64 (`blake3_neon.c`, 6 `unsafe` FFI calls, 13 `unsafe` dispatch
  sites); with `pure` its build script compiles nothing, every `unsafe` block
  is cfg'd out on aarch64, and the code that runs (`portable.rs`, `lib.rs`,
  `guts.rs`) has none. It brings `arrayvec` and `constant_time_eq` (new to the
  tree) and `cfg-if`, plus `cc` at build time (already present). Writing it
  ourselves was considered and declined: a published standard with no
  secrets in play is checkable, but the authors' code is the most reviewed
  there is.
- **XXH3-128: ours** (`src/xxh3.rs`), next to the codec's XXH64, written from
  xxHash 0.8.2's reference — seed 0 and the default secret only, one-shot and
  streaming. Every Rust port carries `unsafe` for reads `first_chunk` does
  safely, and none is the authors'.
- **The oracle is xxHash's own C.** zstd vendors `xxhash.h` with XXH3 switched
  off by a "local adaptations" block; `zsta-seekable-probe/c/xxhash.h` is that
  file with the block removed, compiled by the probe's build script. Our XXH3
  matches it on every length to 2200 and at the buffer and block boundaries
  beyond, whole and in every piece size the streaming buffer cares about, and
  over 5.2 M runs of a new fuzz target, `archive_hash`. Six deliberate bugs —
  one per path — all failed it. The empty input also matches xxHash's
  published value (`99aa06d3…497f`), which depends on neither.

**Where hashing happens.** The writer hashes content as it is read, chunk by
chunk for chunked members. `copy_entry` hashes as it streams, and fails after
the last byte, so extraction now removes a file whose content failed —
hash, checksum or decode — rather than leaving it complete-looking. `verify`
hashes inside its window loop: members tile the stream, so at most one is
mid-hash when a window ends, and it carries over. A test with a 3 MiB member
and a 160 KiB cache still decodes 1.0x. Appending keeps the archive's
algorithm (an index has one). Recovery hashes what it keeps as its tar walk
passes, with the options' algorithm, so a recovered archive's records equal
the original's; a mutant that dropped the bytes carried between frames
failed two oracle tests.

The probe's independent §7 reader now parses `HASH` too, and the oracle
tests check every digest against the content as libzstd decodes it — never
through our reader — hashed by the `blake3` crate or the reference C.

**What it cost, single-threaded** (2026-10-14; `corpus/`, three interleaved
rounds, medians; swap 84–87%, language servers paused; each run 0.1–0.3 s, so
single figures are rough but the BLAKE3 penalty was the same in all three):

| operation | none | XXH3 | BLAKE3 | `gtar \| zstd -3` |
|---|---:|---:|---:|---:|
| create | 1.00x | 0.83x | 1.55x | 0.45x |
| verify | 1.00x | 1.09x | 2.10x | 0.43x |
| extract | 1.00x | 1.02x | 1.85x | 0.51x |

XXH3 is lost in the noise. BLAKE3 added a steady 0.10–0.12 s per 59 MB in
every operation — the `pure` build hashing about 500 MB/s on one core, slower
than zalt decodes, while everything around it ran on every core. (This corpus
is mostly one 61 MB file, where zalt's slower decode dominates the comparison
with `gtar | zstd`; it is not comparable with the registry bench.)

**So BLAKE3 now runs on every core** (`hash::ParallelBlake3`), without new
dependencies or `unsafe`: the crate's `hazmat` module — always built,
`pure` included — computes the chaining value of any aligned power-of-two
subtree, and merges them. Content is cut into 256 KiB pieces; batches of
four per thread are hashed in parallel, each at its offset in the input;
the chaining values are merged at the end, splitting each range where
BLAKE3's own tree does (`left_subtree_len`) — the recipe the crate's
documentation gives. A piece is only finalized once more input follows it,
since a piece that turned out to be the whole input is the root and hashed
differently; one piece or less is plain `blake3::hash`. Tests compare with
`blake3::hash` on every tree shape (1 KiB pieces, every length to 3 KiB and
every chunk boundary to 70 chunks, four batch/thread/feeding combinations),
larger pieces, and the production geometry around its piece and batch
edges; `archive_hash` fuzzes it with geometry from the input (1.4 M runs).
Four deliberate bugs — the wrong split, a missing offset, swapped children,
pieces out of order — all failed. **Re-timing is pending** (swap was 93%).

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
4. ~~**Threading model.** One decoder per response, or a shared pool? Determines
   whether instances must be `Send` and whether the budget is per-thread or global.~~
   **Answered:** multithreaded, both shapes, one global budget. Every type is
   `Send` and the pool and budget `Sync`, now asserted; the pool measures at
   parity with a decoder per thread. See M2's threading notes.
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
