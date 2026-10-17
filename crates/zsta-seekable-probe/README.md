# zsta-seekable-probe

This crate answers **Open Question Q1** of the ZSTA spec (`doc/ZSTA_SPEC.md`), and Q7 along the way:

> **Q1.** Confirm the behavior of `contrib/seekable_format` with a leading skippable
> frame and zero-size seek table entries.

It writes real archives in the spec's layout. It then reads them back with zstd's **reference seekable reader** (`contrib/seekable_format`, compiled by `zstd-sys` with its `seekable` feature), the reference streaming decoder, and, when available, the `zstd` CLI and a system `tar`.

## Result

Q1 is **confirmed: keep the §8.2 profile as written**. The unmodified reference reader reads ZSTA archives correctly when every frame is listed:

- the Archive Header is entry 0;
- skippable frames are listed with `Decompressed_Size = 0`;
- the Central Index is the last entry.

| Check | Result |
|---|---|
| Seek table accepted, frame offsets and sizes match the writer | ✅ |
| Offset lookup never lands on a zero-size entry (they are always passed over to the next data frame) | ✅ |
| Full-stream read, every-frame read, frame-boundary reads, 400 random ranges | ✅ |
| `decompressFrame` on zero-size entries returns 0 bytes, no error | ✅ |
| Header plus 3 more skippable frames before the first data frame | ✅ |
| **Q7:** skippable frames *between* data frames, after every 1 or every 3 frames | ✅ |
| 24,003-entry seek table (281 KiB, larger than the reader's 128 KiB buffer), half zero-size | ✅ |
| Empty archive (Terminator only) | ✅ |
| §11.1 indexed open located through the reference reader, then extracting every member | ✅ |
| Checksum value of zero-size entries: `0`, `XXH64("")`, `0xDEADBEEF` all behave the same | never read |
| Control: flip one bit in a *data* frame's checksum | detected (`Data corruption detected`) |
| Stock streaming decode, `zstd -t`, `zstd -dc`, `tar -tf`, `tar -I zstd -tf`, `tar -xf` | ✅ |
| §11.3 recovery scan rebuilds a byte-identical seek table | ✅ |
| **The Q1 fallback** (list only data frames) | ❌ unusable |

The fallback fails because the reference reader computes every file offset from 0. With a data-only table, it places data frame 0 at offset 0, where the Archive Header actually sits. The effects:

- a full read fails with `Data corruption detected` when checksums are on, or `Unknown frame descriptor` when they are off;
- random access to any frame fails.

That test runs in a child process, because the reference C code has live `assert()`s.

### Spec edits (applied in draft 0.2)

1. **§15 Q1:** resolved; the data-frames-only fallback was dropped.
2. **§8.2:** added *"Readers MUST NOT verify the Checksum of zero-size entries."* The writer rule "skippable `Checksum` MUST be 0" stays. The reference reader never reads that column for zero-size entries, but XXH64 of empty input truncates to `0x51D8E999`, not 0, so a reader that verified every entry would reject valid archives.
3. **§3.1:** data frames are now "0 or more". An empty archive (listed in §14.3) is just the Terminator.
4. **§15 Q7:** findings recorded. Interleaved skippable frames work with the reference seekable reader and with stock decoders, so allowing them is a design choice, not a compatibility risk. The decision is still open.
5. **§5.2(3):** frames are capped at 1 GiB instead of 2³²−1 bytes, matching the reference seekable *compressor* (`ZSTD_SEEKABLE_MAX_FRAME_DECOMPRESSED_SIZE`). The reader itself doesn't enforce it, and the §12 default of 64 MiB is far below either limit.
6. **§11.1 step 4:** now notes that the reference reader does **not** check that the table covers the file, so ZSTA readers must perform that check themselves.

The probe writes archives stamped with format version 0.2 (`Major_Version` 0, `Minor_Version` 2), matching the spec.

## Running

```sh
cargo test -- --nocapture
```

- `ZSTD_BIN` / `TAR_BIN` pick the CLI tools; otherwise they are looked up on `PATH`. The CLI test prints `SKIPPED` if `zstd` isn't found. On macOS, `brew install zstd`. The system `tar` there is bsdtar, which the test is written to handle, though it has only been run with GNU tar here.
- `ZSTA_LEVEL` sets the compression level for most tests. The default is 3. The §5.2 test always runs levels −5, 1, 3, 9 and 19.

Tested here with:

- `zstd-sys` 2.1.0+zstd.1.5.7 (`seekable` + `experimental` features, legacy support off);
- zstd CLI 1.5.7;
- GNU tar 1.35;
- rustc 1.95.

## Testing your encoder

Implement `zsta_seekable_probe::encoder::FrameEncoder` for your encoder:

```rust
pub trait FrameEncoder {
    /// Compress `src` into exactly one standalone Zstandard frame.
    fn encode_frame(&mut self, src: &[u8]) -> Result<Vec<u8>, String>;
    fn describe(&self) -> String;
}
```

Then return it from `encoder_at()` in `tests/common/mod.rs`. Every archive in the suite, including the Central Index body, is then written by your encoder and read back by the *reference* decoder and seekable reader. `frame_conformance_section_5_2` is the direct check of §5.2 for each frame:

- exactly one frame per call;
- `Frame_Content_Size` present and correct;
- window ≤ 2²⁷;
- no dictionary ID;
- content checksum present;
- the embedded checksum equals XXH64 of the content.

## Layout

| Path | What it is |
|---|---|
| `src/archive.rs` | Minimal ZSTA writer. `SeekProfile`, `SkippableChecksum`, leading and interleaved skippable frames are all switchable. |
| `src/index.rs` | Central Index encode/decode (§7), strict VarInt (§2), path validation and resource limits (§13) |
| `src/seekable.rs` | Safe wrapper over the reference `ZSTD_seekable_*` API |
| `src/stock.rs` | Plain streaming decode (what `zstd -dc` does) |
| `src/frame.rs` | Frame inspection and the §11.3 recovery scan |
| `src/tar.rs` | pax/ustar writer that records `Header_Offset` and data offsets |
| `src/corpus.rs` | Deterministic corpora: mixed ~5 MiB, 6,000 tiny files, empty |
| `src/xxh64.rs` | Dependency-free XXH64, cross-checked against libzstd's frame checksums |
| `tests/q1_seekable.rs` | The tests |
| `tests/common/mod.rs` | Shared verification, plus `encoder_at()`, the plug-in point |

The writer and index code are deliberately minimal. They follow the spec closely enough to seed the real implementation, but they are not it. For example, they don't emit optional index fields other than `LINK`, and they don't emit xattrs.
