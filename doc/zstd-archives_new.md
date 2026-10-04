# Zstandard Archive Format (ZSTA)

**Version:** 0.2 (draft)
**Status:** Early working draft. Incompatible changes are expected between drafts; there is no 1.0 yet.
**Working name:** "ZSTA" is a placeholder and may be renamed without changing the format.

---

## 1. Introduction

ZSTA is a multi-file archive format built entirely from standard Zstandard frames.
A ZSTA archive is at the same time:

1. **A valid `.tar.zst` file.** Any conformant Zstandard decoder followed by any POSIX tar reader can extract it: `zstd -dc a.tar.zst | tar -x`.
2. **A randomly accessible archive.** A trailing seek table and a central index let a ZSTA-aware reader list the contents without decompressing anything. The reader can extract any single file by decoding only the frames that contain it, and can decode frames in parallel.

ZSTA achieves this through two properties of the Zstandard format (RFC 8878):

- Decoders concatenate the output of consecutive frames.
- Decoders silently skip *skippable frames*.

All ZSTA metadata lives in skippable frames. All file content lives in ordinary Zstandard frames whose concatenated output is a POSIX pax tar stream.

### 1.1 Goals

- Full backward compatibility with stock `zstd` and `tar` tooling. This compatibility is optional only when dictionary mode is used.
- O(1) lookup of the index from the end of the file.
- Listing the contents without decompressing file data.
- Extracting a single file at a cost bounded by the frame size, not the archive size.
- Parallel compression and decompression across independent frames.
- Cheap appends.
- Faithful round-tripping of macOS/APFS metadata: extended attributes, resource forks, BSD file flags, birth time and ACLs.
- Recoverability: a damaged trailer can be rebuilt by scanning the file.

### 1.2 Non-goals (current drafts)

- Encryption and signatures (reserved for a future extension).
- In-place deletion or modification of members (these require a rewrite).
- Content-addressed deduplication across members.
- Legacy Zstandard frame formats (v0.x).

### 1.3 Conventions

- The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT** and **MAY** are to be interpreted as described in RFC 2119.
- All fixed-width integers are **little-endian**, as in Zstandard.
- `u8`, `u16`, `u32` and `u64` denote unsigned integers of 8, 16, 32 and 64 bits.
- Offsets are in bytes and zero-based.
- Two kinds of offset are used throughout:
  - A **file offset** is a position in the archive file.
  - A **stream offset** is a position in the decompressed tar stream, meaning the concatenated output of all data frames.
- Sections marked *(informative)* are guidance, not requirements.

### 1.4 References

- **[RFC8878]** Zstandard Compression and the `application/zstd` Media Type.
- **[SEEKABLE]** Zstandard Seekable Format, `contrib/seekable_format/zstd_seekable_compression_format.md` in the facebook/zstd repository.
- **[PAX]** IEEE Std 1003.1-2001 (POSIX.1-2001), `pax` interchange format.
- **[XXH]** xxHash: XXH64 and XXH3.

---

## 2. Primitive encodings

### 2.1 VarInt

A VarInt is an unsigned integer encoded as LEB128. Each byte carries 7 data bits, least significant group first. The high bit is set on every byte except the last.

- Encoders **MUST** emit the shortest possible encoding.
- Decoders **MUST** reject:
  - encodings longer than 10 bytes;
  - values that overflow 64 bits;
  - non-minimal encodings, meaning a trailing `0x00` group after the first byte.

### 2.2 SVarInt

A signed 64-bit integer, ZigZag-mapped to an unsigned value and then encoded as a VarInt:

```
zz(n) = (n << 1) ^ (n >> 63)
```

### 2.3 Bytes / String

A `VarInt` length followed by that many raw bytes. A *String* is Bytes that **SHOULD** be valid UTF-8.

### 2.4 Skippable frame (from RFC 8878)

```
Offset  Size  Field
0       4     Magic_Number   u32, 0x184D2A50..0x184D2A5F
4       4     Frame_Size     u32, size of User_Data in bytes
8       N     User_Data
```

On disk, `50 2A 4D 18` is magic `0x184D2A50`.

---

## 3. Archive layout

### 3.1 Frame sequence

An archive is a sequence of frames in exactly this order:

```
 #   Frame                         Kind        Magic          Count
───  ────────────────────────────  ──────────  ─────────────  ──────
 1   Archive Header                skippable   0x184D2A50     exactly 1, at file offset 0
 2   Dictionary                    skippable   0x184D2A51     0 or more
 3   Data frames                   zstd        0xFD2FB528     0 or more
 4   Terminator                    zstd        0xFD2FB528     exactly 1
 5   Central Index                 skippable   0x184D2A52     exactly 1
 6   Seek Table                    skippable   0x184D2A5E     exactly 1, last in file
```

Rows 3 and 4 are both ordinary Zstandard frames. The Terminator is simply the last data frame and has a defined content (§5.4). An archive with no entries has no frames in row 3: it consists of the Archive Header, the Terminator, the Central Index and the Seek Table.

Stock tools see this file as follows:

```
zstd decoder:  [skip] [skip]*  DATA DATA ... DATA  TERM  [skip]  [skip]
                               └────── concatenated output ──────┘
tar reader:                    hdr file hdr file ...     EOF (2 × 512 zero bytes)
```

### 3.2 Skippable magic allocation

| Magic | Purpose |
|---|---|
| `0x184D2A50` | Archive Header (§4) |
| `0x184D2A51` | Dictionary (§6) |
| `0x184D2A52` | Central Index (§7) |
| `0x184D2A53`–`0x184D2A5D` | Reserved for future ZSTA versions |
| `0x184D2A5E` | Seek Table (§8), as defined by [SEEKABLE] |
| `0x184D2A5F` | Vendor/private. User_Data begins with a 4-byte vendor tag. |

Placement rules:

- Writers **MUST NOT** place skippable frames anywhere other than the positions listed in §3.1 (but see Q7).
- Readers that scan the file (§11) **MUST** skip skippable frames with reserved or vendor magics.

### 3.3 File naming and media type *(informative)*

The recommended extension is `.tar.zst`. The file is a valid instance of `application/zstd`.

---

## 4. Archive Header frame

The Archive Header is the first frame in the file, at file offset 0, with Magic `0x184D2A50`. Its User_Data is laid out as follows:

```
Offset  Size  Field
0       4     Archive_Magic     "ZSTA" (0x5A 0x53 0x54 0x41)
4       1     Major_Version     = 0
5       1     Minor_Version     = 2
6       2     Archive_Flags     u16
8       ...   Header_TLVs       zero or more TLV records, to end of User_Data
```

### 4.1 Version rules

The on-disk version is the draft version: this document describes format **0.2**.

- While `Major_Version` is 0, the format is unstable and any draft may change it incompatibly. Readers **MUST** reject archives unless both `Major_Version` and `Minor_Version` match a version they implement.
- The rules for 1.0 and later are not yet in force. The intended rules are:
  - Readers reject archives whose `Major_Version` is not a version they implement.
  - Readers may read archives with a higher `Minor_Version`, ignoring unknown TLVs and unknown index extensions (§7.4).
  - A minor version bump does not change the meaning of existing fields.

### 4.2 Archive_Flags

| Bit | Name | Meaning |
|---|---|---|
| 0 | `DICT_REQUIRED` | At least one data frame references a dictionary from §6. Stock decoders cannot decode this archive. |
| 1–15 | reserved | **MUST** be 0 when writing. Readers **MUST** reject unknown bits that are set. |

`Archive_Flags` sits at a fixed offset so that appenders can patch it in place (§10).

### 4.3 Header TLVs

Each TLV record has this layout:

```
Offset  Size  Field
0       2     Tag      u16
2       2     Length   u16
4       L     Value
```

| Tag | Name | Value |
|---|---|---|
| `0x0001` | Creator | String bytes (no length prefix), e.g. `"myzstd 0.9 arm64"` |
| `0x0002` | Created | `u64` Unix time in nanoseconds (UTC) |
| `0x0003` | Writer_Policy | Informative bytes describing the frame policy; readers **MUST NOT** depend on it |
| `0x0004`–`0x7FFF` | reserved | Ignored by readers |
| `0x8000`–`0xFFFF` | private | Implementation-defined; ignored by other readers |

---

## 5. Data frames and the tar stream

### 5.1 Tar stream

The concatenated decompressed output of all data frames, in file order and including the Terminator, is the **tar stream**. The tar stream:

- **MUST** be a valid POSIX.1-2001 pax archive, with ustar magic `"ustar\0"` and version `"00"`.
- **MUST** have a length that is a multiple of 512 bytes.
- **MUST** end with the Terminator content (§5.4) and contain no end-of-archive marker before it.

### 5.2 Data frame requirements

Every data frame **MUST**:

1. Be a Zstandard frame conformant to [RFC8878], with magic `0xFD2FB528`. Legacy frames are forbidden.
2. Have the `Frame_Content_Size` field present.
3. Decompress to a length that is a multiple of 512 and no larger than 2³⁰ bytes (1 GiB). This is the largest frame the reference [SEEKABLE] compressor produces; the seek table itself could describe frames up to 2³² − 1 bytes.
4. Begin at a stream offset that is a multiple of 512. This follows from rule 3.
5. Not require a window larger than 2²⁷ bytes (128 MiB). This keeps the archive decodable by stock `zstd` without extra memory flags.
6. Reference a dictionary only when `DICT_REQUIRED` is set. Such a frame **MUST** carry a non-zero `Dictionary_ID` that matches a Dictionary frame in the same archive.

In addition, data frames:

- **SHOULD** set `Content_Checksum_Flag`.
- **SHOULD** be no larger than 64 MiB when decompressed (§12).

Encoders **MAY** use any internal strategy, including platform-specific SIMD paths. The only requirement is that the emitted bytes decode identically under the reference decoder. This is the conformance boundary for custom encoders.

### 5.3 Entry placement

The following terms describe where an entry sits in the tar stream:

- An **entry** is one tar member. It consists of:
  - zero or more pax extended headers (typeflag `x`), each followed by its data blocks;
  - the ustar header;
  - the content blocks.
- The entry's **header offset** is the stream offset of its first header block, which is the pax `x` header when one is present.
- The entry's **data offset** is the stream offset of its first content block.

Placement rules:

- Writers **SHOULD NOT** split an entry across frames when its total size (headers plus padded content) is no larger than the writer's solid-frame target.
- Larger entries **MAY** be split across frames at any 512-byte boundary.
- A single frame **MAY** contain many entries. Such a frame is a *solid group*.
- Global pax headers (typeflag `g`) **SHOULD NOT** be used. If they are used, they **MUST** appear in the first data frame.

### 5.4 Terminator frame

The Terminator is the last data frame. It:

- **MUST** decompress to only zero bytes;
- **MUST** decompress to at least 1024 bytes, in a multiple of 512;
- **MUST NOT** contain any entry data.

Writers **SHOULD** emit exactly 1024 zero bytes.

Isolating the end-of-archive marker in its own frame allows appending without re-encoding existing data (§10).

---

## 6. Dictionary frames

Dictionary frames use Magic `0x184D2A51`. Each frame's User_Data is a single Zstandard dictionary in the *formatted* layout, which starts with magic `0xEC30A437` and has a non-zero `Dictionary_ID`.

- Raw-content dictionaries are not allowed.
- All Dictionary frames **MUST** appear before the first data frame. This lets streaming readers load them before they need them.
- `Dictionary_ID` values **MUST** be unique within an archive.
- If any data frame references a dictionary, `DICT_REQUIRED` **MUST** be set.
- The Central Index body (§7) **MUST NOT** reference a dictionary.

*(Informative)* Dictionary mode trades stock-tool compatibility for a better ratio on many small, similar files. Implementations **SHOULD** offer a conversion that re-encodes a dictionary archive into a portable one.

---

## 7. Central Index frame

The Central Index uses Magic `0x184D2A52`. It is the last frame listed in the seek table (§8.2). Its User_Data is laid out as follows:

```
Offset  Size  Field
0       4     Index_Magic         "ZSTI" (0x5A 0x53 0x54 0x49)
4       1     Index_Version       = 1
5       1     Hash_Algorithm      u8, see §7.3
6       2     Index_Flags         u16, reserved, MUST be 0
8       8     Entry_Count         u64
16      8     Tar_Stream_Size     u64, total stream length including the Terminator
24      N     Index_Body          one Zstandard frame (see below)
24+N    8     Index_Checksum      u64, XXH64 (seed 0) of User_Data bytes [0, 24+N)
```

Index_Body is a single Zstandard frame. It **MUST** have `Frame_Content_Size` and `Content_Checksum_Flag` set, and it **MUST NOT** reference a dictionary. It decompresses to exactly `Entry_Count` Entry Records (§7.1), stored back to back.

Validation:

- Readers **MUST** verify `Index_Checksum`.
- Readers **MUST** check that `Tar_Stream_Size` equals the sum of the data-frame `Decompressed_Size` values in the seek table.

### 7.1 Entry Record

Entry Records appear in tar-stream order, so `Header_Offset` is strictly increasing. There is exactly one record per tar member. Pax `x` headers are folded into the member they describe and do not get their own record.

```
Field               Encoding     Notes
──────────────────  ───────────  ─────────────────────────────────────────────
Typeflag            u8           tar typeflag byte: '0' file, '1' hardlink,
                                 '2' symlink, '3' char dev, '4' block dev,
                                 '5' dir, '6' fifo
Presence            VarInt       bitset of optional fields, see §7.2
Path_Shared         VarInt       bytes shared with the previous record's path
                                 (0 for the first record)
Path_Suffix         Bytes        remaining path bytes
Mode                VarInt       st_mode & 07777
UID                 VarInt
GID                 VarInt
Mtime_Sec           SVarInt      Unix seconds
Mtime_Nsec          VarInt       0..999 999 999
Header_Offset       VarInt       stream offset of the entry's first header block
Data_Delta          VarInt       data offset − Header_Offset
Size                VarInt       stored content bytes in the tar stream
                                 (0 for non-regular entries and for hardlinks)
[optional fields, in bit order, per Presence]
```

Path rules:

- The reconstructed path is `prev_path[0:Path_Shared] ++ Path_Suffix`.
- It **MUST** be identical to the effective tar path, meaning the pax `path` value if present.
- Paths use `/` as the separator, are relative, and **MUST NOT** contain `..` components, empty components, or a leading `/`.
- Directory paths **SHOULD NOT** carry a trailing `/` in the index.

### 7.2 Presence bits

Each set bit adds one optional field, appended in ascending bit order.

| Bit | Name | Field encoding |
|---|---|---|
| 0 | `HASH` | Raw digest. Its length is fixed by `Hash_Algorithm`. It covers the *logical* file content, which for sparse files is the expanded content. |
| 1 | `LINK` | String: link target, for symlinks and hardlinks |
| 2 | `BTIME` | SVarInt seconds, VarInt nanoseconds (birth/creation time) |
| 3 | `NAMES` | String uname, String gname |
| 4 | `FFLAGS` | VarInt `st_flags` (BSD file flags) |
| 5 | `XATTRS` | VarInt count, then for each: String name, VarInt value size. Values are stored only in the tar stream (§9.2). |
| 6 | `DEVICE` | VarInt major, VarInt minor |
| 7 | `SPARSE` | VarInt logical size. `Size` is then the stored, non-hole size. |
| 8 | `EXT` | Bytes containing Extension TLVs (§7.4) |
| 9+ | reserved | **MUST** be 0. Readers **MUST** reject a record with unknown bits set. |

New presence bits require a new `Index_Version`. Forward-compatible additions go in `EXT`.

### 7.3 Hash algorithms

| Value | Algorithm | Digest length |
|---|---|---|
| 0 | none (the `HASH` bit **MUST NOT** be set) | 0 |
| 1 | XXH3-128 | 16 |
| 2 | BLAKE3-256 | 32 |
| 3 | SHA-256 | 32 |

Hashes provide integrity, not authenticity (§13).

### 7.4 Extension TLVs (inside `EXT`)

Each record is `VarInt tag`, `VarInt length`, then `value`. Readers **MUST** skip tags they don't recognize. No tags are defined yet. Tags ≥ `0x4000` are private.

### 7.5 Authority

The tar stream is **authoritative**. The index is a cache for listing and locating entries.

- Writers **MUST** keep the index consistent with the tar stream.
- Readers **MAY** cross-check the two.
- Readers **SHOULD** report an error on any inconsistency.
- When restoring metadata, readers **SHOULD** use the tar headers.

---

## 8. Seek Table frame

The Seek Table is defined by [SEEKABLE] and uses skippable Magic `0x184D2A5E`. It **MUST** be the last frame in the file.

### 8.1 Layout (per [SEEKABLE])

```
Skippable header:  u32 Magic (0x184D2A5E), u32 Frame_Size
Seek_Table_Entries[Number_Of_Frames]:
    u32 Compressed_Size
    u32 Decompressed_Size
    u32 Checksum             present only if Checksum_Flag is set
Seek_Table_Footer (9 bytes):
    u32 Number_Of_Frames
    u8  Seek_Table_Descriptor   bit 7 = Checksum_Flag; bits 2–6 reserved (0); bits 0–1 unused
    u32 Seekable_Magic_Number   0x8F92EAB1
```

### 8.2 ZSTA profile of the seek table

The seek table **MUST** list **every frame** from file offset 0 up to and including the Central Index, in file order. The Seek Table frame itself is not listed. Therefore:

- `Compressed_Size` is the full on-disk size of the frame. For a skippable frame this is `8 + Frame_Size`.
- For skippable frames, `Decompressed_Size` **MUST** be 0, and `Checksum` (if present) **MUST** be 0.
- For data frames:
  - `Decompressed_Size` **MUST** equal the frame's `Frame_Content_Size`.
  - `Checksum`, if present, **MUST** be the low 32 bits of XXH64 (seed 0) of the decompressed content, as in [SEEKABLE].
- The first entry is the Archive Header.
- The last entry is the Central Index.
- Writers **SHOULD** set `Checksum_Flag`.
- Readers **MUST NOT** verify the `Checksum` of zero-size entries. The required value 0 is not the XXH64 of empty input (whose low 32 bits are `0x51D8E999`), so a reader that verified every entry would reject valid archives.

With this profile, the file offset of frame *i* is the sum of `Compressed_Size` over entries 0 to *i*−1. The stream offset of frame *i* is the sum of `Decompressed_Size` over the same range. Both are computable with no other information.

*(Informative)* This profile works with the unmodified reference [SEEKABLE] reader. It was tested against the reader in zstd 1.5.7 (see resolved question Q1, §15.2):

- Offset lookup always passes over zero-size entries and resolves to the non-empty data frame that holds the offset.
- `ZSTD_seekable_decompressFrame` on a zero-size entry returns 0 bytes without error.
- The reader never reads the `Checksum` of zero-size entries, while it does verify the `Checksum` of data frames.
- A table that lists only data frames does **not** work. The reader derives every file offset by summing `Compressed_Size` from 0, so it misplaces every frame by the size of the leading skippable frames.

---

## 9. Tar profile

### 9.1 Required pax records

Writers **MUST** emit a pax `x` header whenever a value doesn't fit the ustar fields. This includes:

- paths longer than 100 bytes, or 255 bytes with the ustar prefix;
- sizes of 8 GiB or more;
- UID/GID values beyond octal range;
- non-ASCII names;
- sub-second times.

The records used are:

| Pax keyword | Use |
|---|---|
| `path`, `linkpath` | Long or non-ASCII names |
| `size` | Large files |
| `uid`, `gid`, `uname`, `gname` | Ownership |
| `mtime` | With fractional seconds when the source has them |
| `LIBARCHIVE.creationtime` | Birth time |
| `SCHILY.fflags` | BSD file flags, in libarchive textual form (e.g. `uchg,hidden`) |
| `SCHILY.xattr.<name>` | Extended attributes, raw value bytes |
| `SCHILY.acl.ace` | NFSv4-style ACLs (the macOS ACL model), in libarchive textual form |
| `GNU.sparse.*` (format 1.0) | Sparse files (optional) |

### 9.2 Extended attributes

- Writers **MUST** encode xattrs as `SCHILY.xattr.<name>`.
- Writers **MAY** additionally emit `LIBARCHIVE.xattr.<url-encoded-name>` with base64 values.
- Readers **MUST** accept both forms. If both are present for the same name, `SCHILY` wins.

### 9.3 macOS specifics

- **Transparent compression.** `com.apple.decmpfs` **MUST NOT** be archived. Content of files with `UF_COMPRESSED` **MUST** be read through the normal file API, so it is stored decompressed. `UF_COMPRESSED` **MUST** be cleared from the recorded flags.
- **Resource forks.** `com.apple.ResourceFork` is archived as an ordinary xattr. Writers **MUST NOT** emit AppleDouble `._` members.
- **Quarantine and provenance.** Whether to archive `com.apple.quarantine` and `com.apple.provenance` is writer policy. Readers **SHOULD** apply `com.apple.quarantine` to extracted files when the archive itself is quarantined.
- **Unicode normalization.** Paths are stored exactly as returned by the filesystem, with no normalization. Readers **MAY** normalize for lookup and comparison. They **MUST NOT** rewrite paths in the archive.

### 9.4 Ordering

- Directory entries **SHOULD** precede their contents.
- The first occurrence of a hardlinked inode **MUST** be a regular file carrying the content. Later occurrences are typeflag `1` with `linkpath` pointing to the first.

---

## 10. Appending

To append entries to a complete archive:

1. Locate the Terminator. It is the data frame immediately before the Central Index in the seek table.
2. Truncate the file at the Terminator's file offset.
3. Write the new data frames, then a new Terminator.
4. Write a new Central Index containing all old and new records. Old records are unchanged, since their offsets don't move.
5. Write a new Seek Table.
6. If the append adds dictionary-using frames, patch `Archive_Flags` in place.

New Dictionary frames cannot be added by appending, because §6 requires them before the first data frame. Adding one requires a rewrite.

*(Informative)* Between steps 2 and 5, the file has no trailer. A crash in that window is recoverable by scanning (§11.3). Writers that need atomicity **SHOULD** append to a copy and rename it into place.

Deletion and in-place replacement are not supported. They require rewriting, which is compaction.

---

## 11. Reading

### 11.1 Indexed open

1. Read the last 9 bytes. Verify the magic `0x8F92EAB1` and that the reserved descriptor bits are 0.
2. Compute the entry size, which is 12 if `Checksum_Flag` is set and 8 otherwise. The seek table frame length is then `8 + Number_Of_Frames × entry_size + 9`.
3. Read the seek table frame and verify its skippable header (magic `0x184D2A5E`, correct `Frame_Size`).
4. Verify that the sum of all `Compressed_Size` values plus the seek table frame length equals the file size. The reference [SEEKABLE] reader does not perform this check, so readers built on it **MUST** perform it separately.
5. Read the frame at offset 0 and validate the Archive Header (§4).
6. Read the last listed frame and validate the Central Index (§7).
7. Load all Dictionary frames if `DICT_REQUIRED` is set.

### 11.2 Extracting one entry

1. Look up the Entry Record for the path.
2. Compute the byte range to decode: `[Header_Offset, Header_Offset + Data_Delta + pad512(Size))`.
3. Find the data frames whose stream ranges intersect that range.
4. Decode those frames. Frames are independent, so they can be decoded in parallel.
5. Parse the tar headers at `Header_Offset` and write out the content.
6. If present, verify the frame checksum and the Entry `HASH`.

### 11.3 Recovery scan

If the trailer is missing or invalid, a reader **MAY** rebuild it:

1. Walk the file from offset 0. Every frame is self-delimiting:
   - skippable frames through `Frame_Size`;
   - Zstandard frames through their frame header and block headers.
2. Rebuild the seek table from the walk.
3. Parse the tar stream to rebuild the index.
4. Treat the last data frame that decodes to only zero bytes as the Terminator.

### 11.4 Streaming read

A reader that cannot seek decodes the file from the start:

1. Parse the Archive Header.
2. Load any dictionaries.
3. Feed the decompressed stream to a tar parser, ignoring the trailing skippable frames.

---

## 12. Writer guidance *(informative)*

### 12.1 Frame policy

| Parameter | Suggested default | Rationale |
|---|---|---|
| Small-file threshold | 256 KiB | Entries below this go into solid groups |
| Solid frame target | 4 MiB | Better cross-file ratio, while extracting one file still costs ≤ 4 MiB of decoding |
| Large-file chunk | 32 MiB | Enables parallel decode and seeking within a file |
| Max frame (decompressed) | 64 MiB | Stays under the §5.2 window limit and limits worst-case latency |

### 12.2 Improving the ratio

- Group small files by extension or type before packing solid frames.
- Put large, incompressible content (media, already-compressed files) in its own frames. Use a low level, or Zstandard raw blocks.
- For corpora of many tiny similar files, consider dictionary mode.

### 12.3 Parallelism

- Frames are independent, so compress them on a work queue across cores. On Apple Silicon, the P-cores and E-cores can process frames concurrently.
- Emit frames in order, since the seek table must be in file order.
- Pipeline tar serialization, compression and I/O.

### 12.4 Reproducibility

With these conditions, the same input produces byte-identical archives:

- a fixed traversal order, such as sorting by byte-wise path;
- a fixed frame policy;
- a fixed encoder build and level;
- the `Created` TLV omitted.

Output is not reproducible across different encoder implementations.

---

## 13. Security considerations

Readers **MUST**:

- **Paths.** Reject or sanitize absolute paths, `..` components and empty components, in both the index and the tar headers.
- **Links.** Never follow an extracted symlink when creating later entries. Either defer symlink creation to the end or use `openat`-style no-follow creation. Hardlink targets **MUST** resolve inside the extraction root.
- **Special files.** Refuse device nodes and setuid/setgid bits unless the caller explicitly opts in.
- **Resource limits.** Enforce limits on:
  - index body decompressed size;
  - `Entry_Count`;
  - path length;
  - xattr count and size;
  - total extracted size;
  - the expansion ratio per frame.
- **Index trust.** Treat index offsets as untrusted. Bounds-check them against `Tar_Stream_Size` and the seek table.

Hashes and checksums detect corruption, not tampering. Authentication is out of scope for now (see Q3).

---

## 14. Conformance

### 14.1 Writer

A conformant writer produces files satisfying §3–§9. In particular, every output **MUST** pass all of the following:

- `zstd -t` (portable profile);
- `zstd -dc file | tar -t` with GNU tar and with bsdtar (portable profile);
- a round trip through this spec's indexed reader with identical content and metadata.

### 14.2 Reader classes

| Class | Capability |
|---|---|
| **R0** | Stock stream reader: any RFC 8878 decoder plus a POSIX tar reader. Works on portable-profile archives. |
| **R1** | Indexed reader: implements §11.1–§11.2 and §13. |
| **R2** | R1 plus recovery (§11.3) and append (§10). |

### 14.3 Test corpus *(to be built)*

The `zsta-seekable-probe` crate provides initial coverage: the seek table against the reference reader, stock-tool compatibility, the empty archive, long and non-ASCII paths, §5.2 frame conformance and the §11.3 recovery scan.

The full test corpus should cover:

- Structure:
  - an empty archive (Terminator only);
  - a single zero-byte file;
  - a single file larger than 4 GiB.
- Paths:
  - long paths, non-ASCII paths, and paths in both NFC and NFD;
  - malicious paths (`../`, absolute paths, symlink escapes), which must be rejected.
- Links: symlinks, hardlinks, and dangling links.
- macOS metadata:
  - xattrs including large resource forks;
  - BSD flags (`uchg`, `hidden`);
  - ACLs;
  - decmpfs-compressed sources.
- Profiles and operations:
  - dictionary mode;
  - appending to an existing archive.
- Damage:
  - a truncated trailer, as for crash recovery;
  - a corrupted index checksum;
  - a corrupted data frame.
- Differential tests: the custom encoder against the reference `libzstd` decoder, at every level and frame size.
- Fuzzing of the trailer parser, the index parser, and the VarInt decoder.

---

## 15. Questions

Question numbers are permanent; resolved questions move to §15.2.

### 15.1 Open

- **Q2.** Should small xattr values (e.g. ≤ 256 B) be inlined in the index, so metadata listing works without decoding data frames?
- **Q3.** How should encryption and signing be added? A candidate is a signed Central Index that covers per-frame hashes, in a new skippable frame.
- **Q4.** Should the index support content-level deduplication, with multiple entries referencing the same stored data, even though tar has no equivalent? A possible approach is an R1-only extension with a tar fallback.
- **Q5.** Should the format support per-group dictionary selection, e.g. one dictionary per file type, with the choice recorded in an `EXT` TLV?
- **Q6.** What should the default `Hash_Algorithm` be: XXH3-128 for speed, or BLAKE3 as a cryptographic option?
- **Q7.** Should vendor skippable frames be allowed between data frames, for example for per-group metadata?
  - *Findings (0.2):* they work with the reference seekable reader when listed as zero-size entries, including one after every data frame in a 24,003-entry seek table, and with stock streaming decoders. This is a design decision, not a compatibility risk.
  - Until it is decided, §3.2 keeps them out. If they are allowed, §10 step 1 must locate the Terminator as the last *data* frame before the Central Index, not the frame immediately before it.

### 15.2 Resolved

- **Q1** *(resolved in 0.2)*. Does `contrib/seekable_format` work with a leading skippable frame and zero-size seek table entries?
  - **Yes.** It was tested with the reference reader from zstd 1.5.7 (through `zstd-sys` 2.1.0), the `zstd` 1.5.7 CLI and GNU tar 1.35, using the `zsta-seekable-probe` crate.
  - The §8.2 profile stays as written, with the added rule that readers do not verify checksums of zero-size entries.
  - The fallback (list only data frames and store the data-region base offset in the Central Index) was dropped, because the unmodified reference reader cannot read such archives.

---

## Appendix A. Example frame map *(informative)*

This example archive has three small files and one 70 MiB file. The frame sizes are illustrative.

```
#  Frame            Csize      Dsize        Stream range            Contents
─  ───────────────  ─────────  ───────────  ──────────────────────  ──────────────────────────
0  Archive Header   8+24       0            —                       "ZSTA" v0.2, flags 0, Created
1  Data             1 210 334  3 145 728    [0, 3 145 728)          a.txt, b.json, c.plist (solid)
2  Data             9 876 001  33 554 432   [3 145 728, 36 700 160) big.bin: pax+header, first chunk
3  Data             9 870 112  33 554 432   [36 700 160, 70 254 592)
4  Data             1 802 947  6 291 968    [70 254 592, 76 546 560)
5  Terminator       17         1 024        [76 546 560, 76 547 584)
6  Central Index    8+196      0            —                       4 records
7  Seek Table       8+84+9     —            —                       7 entries × 12 B, footer
```

To extract `b.json`, a reader decodes only frame 1. To extract bytes 40–41 MiB of `big.bin`, it decodes only frame 3.

---

## Changelog

- **0.2-draft** (2026-09-28):
  - Drafts renumbered: the initial draft is 0.1, and there is no 1.0 yet. The on-disk version is now 0.2 (`Major_Version` 0, `Minor_Version` 2), and §4.1 requires an exact version match while the major version is 0.
  - Resolved Q1: kept the §8.2 all-frames seek table profile and dropped the data-frames-only fallback.
  - §8.2: readers **MUST NOT** verify checksums of zero-size entries.
  - §3.1: an archive may have no data frames before the Terminator.
  - §5.2: frame content is capped at 1 GiB (was 2³² − 1 bytes), matching the reference seekable compressor.
  - §11.1: noted that the reference reader does not check that the seek table covers the file.
  - Q7: recorded test findings; the decision is still open.
  - Replaced "v1" wording with version-neutral wording.
- **0.1-draft**: Initial draft (originally labelled 1.0-draft).
