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

`zalt(1)` compresses, decompresses and inspects zstd frames, and makes and
reads ZSTA archives. Build it once and call the binary, or go through `just`,
which builds in release mode first:

```sh
cargo build --release -p zalt-cli   # → target/release/zalt
just zalt help                      # the same binary, via just
```

`zalt help` and `zalt archive help` print every option; what follows is how
they fit together.

#### Compressing

```sh
zalt c -o notes.txt.zst notes.txt   # level 3, the default
zalt c -19 -o notes.txt.zst notes.txt
zalt c -L 9 notes.txt > notes.txt.zst
tar -c dir | zalt c > dir.tar.zst   # no input file: reads stdin
```

- **Levels run 1 to 19**, as in `zstd(1)`: `-1` … `-19`, or `-L N`. Each
  comes out about the size of `zstd` at the same level, often a little
  smaller. 16 and up parse optimally and are much slower. `zstd`'s 20–22 are
  refused: they exist to declare windows past 8 MiB.
- **Every frame is one an untrusted decoder accepts.** No level declares a
  window above 8 MiB, the ceiling browsers decode. `--max-window-log N`
  lowers that (it never raises a level's own window).
- `--no-check` leaves out the content checksum (on by default, as in `zstd`).
  `--stored` writes raw and RLE blocks only, which is useful for testing.
- Each input becomes **one frame, read whole into memory**. Several inputs go
  to stdout as consecutive frames, which decode to their concatenation; `-o`
  takes one input.
- It refuses to write compressed data to a terminal. Redirect, or use `-o`.

#### Decompressing

```sh
zalt d -o notes.txt notes.txt.zst   # also: zalt -d
zalt d notes.txt.zst | less
zalt d < notes.txt.zst > notes.txt  # stdin to stdout
zalt d -f -o notes.txt notes.txt.zst  # -f to overwrite
```

Decoding streams, so memory does not grow with the file. Frames from any
`zstd` encoder decode, with **one limit**: by default a frame may declare at
most an 8 MiB window, the ceiling appropriate to network input. A large file
made with `zstd --long`, or `--ultra -20` and up, can declare more and is
then refused.
`--trusted` lifts the ceiling for files you made yourself, and
`--max-window-log N` sets it exactly. `--max-decoded SIZE` refuses frames that
expand past a size, and `--budget SIZE` caps total decoder memory. Sizes
accept suffixes: `512`, `64K`, `8MiB`, `1G`.

#### Inspecting

```sh
zalt info notes.txt.zst          # frame and block structure, without decoding
zalt check *.zst                 # decode and discard; non-zero if any input fails
zalt d --in-buf 1 notes.txt.zst  # decode a byte at a time, exercising every boundary
zalt version                     # version, and the decode backend (neon or scalar)
```

#### Archives

A ZSTA archive is a `.tar.zst` that also carries an index, so one member can
be listed or extracted without decoding the rest. Any stock tool still reads
it: `zstd -dc a.tar.zst | tar -x`. `zalt a` is short for `zalt archive`, and
create, list, extract, verify and append are `c`, `t`, `x`, `v` and `r`.

```sh
zalt archive create -o src.tar.zst src/ README.md   # zalt a c
zalt a c -19 --threads 4 -o src.tar.zst src/
zalt a c --hash xxh3 -o src.tar.zst src/     # faster hash; see below
zalt a t src.tar.zst                    # list paths
zalt a t -l src.tar.zst                 # type, mode, uid, gid, size, mtime
zalt a x src.tar.zst -C out/            # extract everything into out/
zalt a x src.tar.zst src/main.rs docs/  # only these, and anything under them
zalt a v src.tar.zst                    # check every frame and every hash
zalt a r src.tar.zst CHANGELOG.md       # append
```

- **Creating.** Paths are stored as given, minus a leading `/`, as tar
  stores them. Frames compress in parallel, one thread per core by default.
  `-f` overwrites an existing archive.
- **Every file's content is hashed**, and the hash is checked whenever the
  file is read: by `verify`, and on extraction, which removes a file that
  fails rather than leave it looking complete. The default is BLAKE3, a
  cryptographic hash, so that a future signed index or deduplication can rely
  on it. `--hash xxh3` is faster to compute and catches accidental damage just as
  well, but not deliberate tampering; `--hash none` leaves hashes out (every
  frame still has its checksum).
- **macOS metadata is kept by default:** extended attributes (resource forks
  included), ACLs and BSD file flags (`hidden`, `uchg`, …). `--no-xattrs`,
  `--no-acls` and `--no-fflags` leave each out. `com.apple.provenance` is left
  out unless `--keep-provenance`, because it only means something on the
  machine that wrote it.
- **Extraction is conservative.** It never writes outside `-C DIR`, whatever
  the paths or links in the archive say. It refuses to overwrite without `-f`
  and strips setuid, setgid and sticky bits without `--keep-setid`. It
  restores owners only with `--same-owner` and creates device nodes only with
  `--devices` (both normally need root). A quarantined archive passes its
  quarantine on to what it extracts.
- **Appending** adds members after the last one and takes `create`'s options,
  except `--hash`: new members get the archive's own. It works on a copy that replaces the archive only once complete, so a
  failure leaves the archive as it was. A path already in the archive is
  refused: members are never replaced.

**Recovering a damaged archive.** An archive whose write was cut short, or
which has damaged frames, will not open. `recover` rebuilds it, keeping every
member the damage did not touch byte for byte, and lists the ones it lost. The
damaged file is only read. Kept members are hashed again with BLAKE3, since
only the lost index recorded which hash the archive used.

```sh
zalt a recover broken.tar.zst -o fixed.tar.zst
zalt a recover --search broken.tar.zst -o fixed.tar.zst
```

`--search` is for damage that took member headers *and* the index with it: it
resumes at the next frame that starts with a tar header. That is a guess,
because an archived `.tar` file's contents can look like members, so it lists
everything it found that way for you to check.

#### Exit status

`0` success; `1` an input was refused or failed (including a refused
overwrite or a duplicate append); `2` the command line was wrong, including a
level above 19. Commands that report — `info`, `check`, `version`,
`archive list` — write the report to stdout. Commands that produce files
write their summaries and per-member lines to stderr, and `-q` silences
those.

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
