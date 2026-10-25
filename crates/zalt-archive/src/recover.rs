//! Recovery (§11.3): rebuilding an archive that lost its trailer, or has
//! damaged frames, from whatever of it is still intact.
//!
//! An archive loses its trailer when a write stops early — a crash during
//! [`crate::append`](fn@crate::append), a writer dropped unfinished, a copy
//! cut short — or when the trailer itself is damaged. A frame is damaged by
//! anything from a bad sector to a flipped bit. Recovery handles both, and
//! never writes to the damaged file.
//!
//! ## Two passes
//!
//! 1. **Where the frames are.** From the seek table if it survived — it lists
//!    every frame's length even where the frame itself is unreadable — and
//!    otherwise by walking from offset 0: every frame is self-delimiting
//!    through its headers ([`zalt_codec::frame::compressed_size`]). A Central
//!    Index found either way is kept for pass 2.
//! 2. **What the frames hold.** Each data frame is decoded — its content
//!    checksum checked, and the seek table's too when there is one — and the
//!    tar stream parsed across them, as §11.3 says.
//!
//! ## Getting past damage, exactly
//!
//! A damaged frame's *length* in the tar stream is usually still known (the
//! seek table, or the frame's own `Frame_Content_Size`), so the stream
//! offsets of everything after it are too. What may be lost is where the next
//! member starts:
//!
//! - **Damage inside one member's content** — say a middle chunk of a large
//!   file — loses only that member: its header, read before the damage, says
//!   exactly where the next one begins.
//! - **Damage that swallowed member headers** — a solid group, the first
//!   chunk of a file — loses the thread. If the Central Index survived, the
//!   walk picks it up again at the next member the index says starts at a
//!   frame boundary, after checking the tar header there names that member.
//!   Without an index the walk stops — unless the caller asks for a
//!   **search** ([`RecoverOptions::search`]): the walk then takes up the
//!   thread at the next frame that *starts* with a valid tar header. That is
//!   a guess, and can be fooled: an archived `.tar` file is full of valid tar
//!   headers, and a frame that begins with one of them is indistinguishable
//!   from a frame that begins a member. So it is opt-in, and every member it
//!   finds is reported in [`Recovered::found_by_search`].
//!
//! ## Dictionaries
//!
//! A dictionary archive (§6) is recovered like any other: its Dictionary
//! frames, which precede every data frame, are loaded before the walk and
//! kept as they were. One that is damaged is dropped, and the frames written
//! against it fail to decode — damage, like any other, costing the members in
//! them. The new Terminator is written without a dictionary, whatever the
//! options say: recovery cannot add a Dictionary frame (§6 puts them first).
//!
//! ## What is kept
//!
//! The tar stream is cut into *runs*: stretches between member boundaries
//! that are also frame boundaries — with zalt's writer, one solid group, one
//! file, or one chunked file. A run is kept if every frame in it decoded and
//! its members tile it exactly; it is then written through **byte for byte**,
//! never re-encoded, and its members' offsets shifted to where it now sits.
//! A run with any damage is dropped whole: its members are the damaged ones.
//!
//! The archive ends at a Terminator — a frame that starts at a member
//! boundary and decodes to whole zero blocks (§5.4) — or, if none is found,
//! after the last complete run. §11.3's own rule, "the last data frame that
//! decodes to only zero bytes", also matches a middle chunk of an all-zero
//! file; the tar stream is authoritative (§7.5), so it decides instead. See
//! `doc/ZALT-MAC.md`, *Findings for the spec*.

use std::io::Write;

use zalt_codec::params::DecodeLimits;

use crate::dict::{self, Dictionary};
use crate::frames::{self, MAX_FRAME_CONTENT, MAX_WINDOW_LOG};
use crate::hash::Hasher;
use crate::index::{Entry, Index, IndexLimits};
use crate::layout::{
    self, FLAG_DICT_REQUIRED, Header, MAGIC_DICTIONARY, MAGIC_HEADER, MAGIC_INDEX,
    MAGIC_SEEK_TABLE, SeekEntry, le32, skippable_payload,
};
use crate::meta;
use crate::read::{ReadLimits, Source};
use crate::tar::{self, BLOCK, Member, pad};
use crate::write::{Resume, Summary, Writer, WriterOptions};
use crate::{Error, Result};

/// How hard [`recover_with`] tries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoverOptions {
    /// Where damage took member headers and no Central Index survived to say
    /// where the next member starts, resume at the next frame that begins
    /// with a valid tar header. Off by default: it can mistake the content of
    /// an archived `.tar` for members. What it finds is listed in
    /// [`Recovered::found_by_search`].
    pub search: bool,
}

/// What [`recover`](fn@recover) found, kept and wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    /// Members in the rebuilt archive.
    pub entries: u64,
    /// Members that could not be kept — damaged, or sharing a run with
    /// damage — by path, in archive order when an index survived to name
    /// them. Without one, members whose headers were lost are not known, so
    /// not listed.
    pub lost: Vec<Vec<u8>>,
    /// Data frames that failed to decode or to match their checksum.
    pub damaged_frames: u64,
    /// Whether the tar stream's end was found. If not, the archive had been
    /// cut short, and the recovered one ends at its last complete run.
    pub terminator_found: bool,
    /// Whether the walk lost the tar stream at damage and found it again
    /// through the Central Index.
    pub resynced_by_index: bool,
    /// Members kept that were found by [`RecoverOptions::search`] rather than
    /// by following the tar stream or the index: probably members, possibly
    /// the content of an archived `.tar`. Check them.
    pub found_by_search: Vec<Vec<u8>>,
    /// Bytes of the damaged file written through unchanged.
    pub kept: u64,
    /// The rest of it: damage, an old trailer, incomplete members.
    pub dropped: u64,
    /// Why the walk stopped before the end of the archive body, if it did.
    pub stopped: Option<String>,
    /// The recovered archive.
    pub summary: Summary,
}

// ------------------------------------------------------------- the tar walk

/// A member the walk parsed, with where it ends and whether damage touched it.
struct Found {
    entry: Entry,
    end: u64,
    damaged: bool,
    /// Found after a search resync, so only probably a member.
    searched: bool,
}

/// The tar stream, parsed a frame at a time, with gaps where frames were
/// damaged.
#[derive(Default)]
struct TarWalk {
    /// Bytes from stream offset `at` on that are not yet consumed: the start
    /// of the next member's headers.
    pending: Vec<u8>,
    at: u64,
    /// Content still to pass before the next member starts.
    skip: u64,
    members: Vec<Found>,
    /// Why the tar stream cannot be followed further, until a resync.
    broken: Option<String>,
    /// Whether the thread was last picked up by a search.
    searched: bool,
    /// The `Hash_Algorithm` the recovered index will have, 0 for none.
    algorithm: u8,
    /// Threads a BLAKE3 hash may use.
    threads: usize,
    /// The last member's content hash, while its content is still arriving:
    /// the hasher and how many content bytes are still to come. They are the
    /// first of `skip`, the padding after them.
    hashing: Option<(Hasher, u64)>,
}

impl TarWalk {
    fn at_boundary(&self) -> bool {
        self.broken.is_none() && self.skip == 0 && self.pending.is_empty()
    }

    fn feed(&mut self, mut data: &[u8], max_header: u64) {
        if self.broken.is_some() {
            return;
        }
        let passed = self.skip.min(data.len() as u64);
        if let Some((mut h, left)) = self.hashing.take() {
            let content = left.min(passed);
            h.update(&data[..content as usize]);
            self.hash_continues(h, left - content);
        }
        data = &data[passed as usize..];
        self.skip -= passed;
        if self.skip > 0 {
            return;
        }
        // Members are parsed in place, from the frame itself. Only headers a
        // frame ends in the middle of are carried over, to be joined with the
        // next. (The first version copied every frame into a buffer and
        // removed each member from its front — a shift of the rest of the
        // frame per member, which on a solid group of thousands of small
        // files made recovery ten times slower than `verify`.)
        let joined;
        let stream: &[u8] = if self.pending.is_empty() {
            data
        } else {
            let mut carried = std::mem::take(&mut self.pending);
            carried.extend_from_slice(data);
            joined = carried;
            &joined
        };
        let limits = IndexLimits::default();
        let mut pos = 0usize;
        while pos < stream.len() {
            let rest = &stream[pos..];
            if rest.len() >= BLOCK && rest[..BLOCK].iter().all(|&b| b == 0) {
                self.broken = Some(format!(
                    "end-of-archive blocks at stream offset {} are inside a data frame, \
                     not a Terminator of their own (§5.4)",
                    self.at
                ));
                return;
            }
            let m = match tar::parse_member_prefix(rest) {
                Ok(Some(m)) => m,
                Ok(None) => {
                    if rest.len() as u64 > max_header {
                        self.broken = Some(format!(
                            "the member at stream offset {} has over {max_header} bytes of headers",
                            self.at
                        ));
                        return;
                    }
                    self.pending = rest.to_vec();
                    return;
                }
                Err(e) => {
                    self.broken = Some(format!("the member at stream offset {}: {e}", self.at));
                    return;
                }
            };
            // A record the rebuilt index could not hold, or its own reader
            // would refuse, ends the walk like any other bad header.
            if m.path.len() > limits.max_path || m.xattrs.len() as u64 > limits.max_xattrs_per_entry
            {
                self.broken = Some(format!(
                    "the member at stream offset {} is over the index's limits",
                    self.at
                ));
                return;
            }
            let total = m.header_len + pad(m.size);
            let start = self.at;
            // The content as far as this frame holds it, hashed now; the rest
            // as the frames carrying it arrive (the top of `feed`).
            let hasher = if m.kind.has_content() {
                Hasher::new(self.algorithm, self.threads)
            } else {
                None
            };
            let content_start = pos + m.header_len as usize;
            let content_here = (m.size).min((stream.len() - content_start) as u64);
            let content_left = m.size - content_here;
            if let Some(mut h) = hasher {
                h.update(&stream[content_start..content_start + content_here as usize]);
                self.hashing = Some((h, content_left));
            }
            self.members.push(Found {
                entry: entry_from(m, start),
                end: start + total,
                damaged: false,
                searched: self.searched,
            });
            if let Some((h, left)) = self.hashing.take() {
                self.hash_continues(h, left);
            }
            self.at += total;
            let available = rest.len() as u64;
            if available >= total {
                pos += total as usize;
            } else {
                self.skip = total - available;
                return;
            }
        }
    }

    /// Record the last member's hash if its content is complete, or keep
    /// hashing it.
    fn hash_continues(&mut self, h: Hasher, left: u64) {
        if left > 0 {
            self.hashing = Some((h, left));
        } else if let Some(m) = self.members.last_mut() {
            m.entry.hash = Some(h.finish());
        }
    }

    /// `len` bytes of stream that a damaged frame held, starting at `at`.
    fn gap(&mut self, len: u64, at: u64) {
        if self.broken.is_some() || len == 0 {
            return;
        }
        // A member the damage reaches is not kept, and its hash could not be
        // finished anyway.
        self.hashing = None;
        if self.skip > 0 {
            // Inside a member whose header was read: it is damaged.
            if let Some(m) = self.members.last_mut() {
                m.damaged = true;
            }
            if self.skip >= len {
                // And it still says exactly where the next one starts.
                self.skip -= len;
                return;
            }
        }
        self.broken = Some(format!(
            "member headers were in the damaged frame at stream offset {at}"
        ));
    }

    /// Pick the stream up again at `at`, where a member is known — or, after
    /// a search, believed — to start.
    fn resync(&mut self, at: u64, searched: bool) {
        self.pending.clear();
        self.hashing = None;
        self.skip = 0;
        self.at = at;
        self.broken = None;
        self.searched = searched;
    }
}

/// The index record for a member whose headers start at stream offset `at`.
fn entry_from(m: Member, at: u64) -> Entry {
    let mut e = Entry::new(m.kind, m.path);
    e.mode = m.mode;
    e.uid = m.uid;
    e.gid = m.gid;
    e.mtime_sec = m.mtime_sec;
    e.mtime_nsec = m.mtime_nsec;
    e.header_offset = at;
    e.data_offset = at + m.header_len;
    e.size = m.size;
    e.link = m.link;
    e.btime = m.btime;
    e.names = m.names;
    e.fflags = m
        .fflags
        .map(|f| f & meta::RECORDED_FLAGS)
        .filter(|&f| f != 0);
    e.xattrs = m
        .xattrs
        .iter()
        .map(|(name, value)| (name.clone(), value.len() as u64))
        .collect();
    e.device = m.device;
    e
}

// --------------------------------------------------- pass 1: frame extents

/// Where one frame is.
#[derive(Debug, Clone, Copy)]
struct Extent {
    offset: u64,
    size: u64,
    /// From the seek table, when it survived: content length and checksum.
    listed: Option<(u64, u32)>,
}

/// A zstd frame's length from its headers, reading `src` in growing pieces.
fn zstd_frame_len<S: Source + ?Sized>(src: &S, offset: u64) -> std::result::Result<u64, String> {
    // A data frame's content is at most 1 GiB (§5.2); stored blocks add three
    // bytes per 128 KiB, so no valid data frame is larger than this.
    const LARGEST: u64 = MAX_FRAME_CONTENT + MAX_FRAME_CONTENT / 1024 + (1 << 16);
    let remaining = src.len() - offset;
    let mut want = remaining.min(64 << 10);
    loop {
        let bytes = src
            .read_at(offset, want as usize)
            .map_err(|e| e.to_string())?;
        match zalt_codec::frame::compressed_size(&bytes) {
            Ok(n) => return Ok(n as u64),
            Err(zalt_codec::Error::Truncated) if want < remaining.min(LARGEST) => {
                want = (want * 2).min(remaining).min(LARGEST);
            }
            Err(zalt_codec::Error::Truncated) if want < remaining => {
                return Err("a frame larger than any data frame may be (§5.2)".into());
            }
            Err(zalt_codec::Error::Truncated) => return Err("the file ends inside a frame".into()),
            Err(e) => return Err(format!("not a frame: {e}")),
        }
    }
}

/// Every frame after the Archive Header, up to and including the Central
/// Index — from the seek table if it is intact, else by walking — and why the
/// walk ended early, if it did.
fn extents<S: Source + ?Sized>(src: &S, header_len: u64) -> (Vec<Extent>, Option<String>) {
    let len = src.len();
    let limits = ReadLimits::default();
    if let Ok((table, _)) =
        layout::read_seek_table(len, |at, n| src.read_at(at, n), limits.max_frames)
    {
        if table
            .first()
            .is_some_and(|e| u64::from(e.compressed_size) == header_len)
        {
            let mut offset = 0u64;
            let mut out = Vec::with_capacity(table.len());
            for e in &table {
                out.push(Extent {
                    offset,
                    size: u64::from(e.compressed_size),
                    listed: Some((u64::from(e.decompressed_size), e.checksum.unwrap_or(0))),
                });
                offset += u64::from(e.compressed_size);
            }
            out.remove(0);
            return (out, None);
        }
    }

    let mut out = Vec::new();
    let mut offset = header_len;
    while offset < len {
        let Ok(head) = src.read_at(offset, 8.min(len - offset) as usize) else {
            return (out, Some(format!("unreadable at offset {offset}")));
        };
        if head.len() < 4 {
            return (
                out,
                Some(format!("the file ends inside a frame at offset {offset}")),
            );
        }
        let magic = le32(&head);
        let size = if zalt_codec::magic::is_zstd_skippable(magic) {
            if head.len() < 8 || offset + 8 + u64::from(le32(&head[4..])) > len {
                return (
                    out,
                    Some(format!("the file ends inside a frame at offset {offset}")),
                );
            }
            8 + u64::from(le32(&head[4..]))
        } else {
            match zstd_frame_len(src, offset) {
                Ok(n) => n,
                Err(why) => return (out, Some(format!("at offset {offset}: {why}"))),
            }
        };
        out.push(Extent {
            offset,
            size,
            listed: None,
        });
        offset += size;
        if magic == MAGIC_INDEX || magic == MAGIC_SEEK_TABLE {
            break;
        }
    }
    (out, None)
}

// ---------------------------------------------------- pass 2: frame content

/// The dictionaries a damaged archive still holds: every Dictionary frame
/// before its first data frame that loads, its ID not seen before, within the
/// reader's limit — and the offsets of those frames, to keep them.
fn load_dictionaries<S: Source + ?Sized>(
    src: &S,
    extents: &[Extent],
) -> (Vec<Dictionary>, Vec<u64>) {
    let limit = ReadLimits::default().max_dictionary_bytes;
    let (mut loaded, mut kept, mut held) = (Vec::new(), Vec::new(), 0u64);
    for x in extents {
        let Ok(frame) = src.read_at(x.offset, x.size as usize) else {
            break;
        };
        let magic = if frame.len() >= 4 { le32(&frame) } else { 0 };
        if !zalt_codec::magic::is_zstd_skippable(magic) || x.listed.is_some_and(|(n, _)| n > 0) {
            break;
        }
        if magic != MAGIC_DICTIONARY || held + x.size > limit {
            continue;
        }
        let Ok(payload) = skippable_payload(&frame, MAGIC_DICTIONARY) else {
            continue;
        };
        let Ok(d) = Dictionary::parse(payload.to_vec()) else {
            continue;
        };
        if dict::find(&loaded, d.id()).is_some() {
            continue;
        }
        held += x.size;
        kept.push(x.offset);
        loaded.push(d);
    }
    (loaded, kept)
}

fn data_limits() -> DecodeLimits {
    DecodeLimits {
        max_window_log: MAX_WINDOW_LOG,
        max_decoded_size: Some(MAX_FRAME_CONTENT),
    }
}

/// A data frame's content, or why it has none to give: undecodable, or not
/// matching what the seek table recorded for it.
fn decode_data_frame(
    frame: &[u8],
    listed: Option<(u64, u32)>,
    dictionaries: &[Dictionary],
) -> std::result::Result<Vec<u8>, String> {
    let declared = frames::header_of(frame, &data_limits())
        .map_err(|e| e.to_string())?
        .frame_content_size
        .ok_or("a data frame without Frame_Content_Size (§5.2)")?;
    let content =
        frames::decode_exactly(frame, declared, dictionaries).map_err(|e| e.to_string())?;
    if let Some((size, checksum)) = listed {
        if size != content.len() as u64 || frames::seek_checksum(&content) != checksum {
            return Err("decodes, but not to what the seek table recorded".into());
        }
    }
    Ok(content)
}

/// How much tar stream a damaged frame held: the seek table's word if it has
/// one, else the frame's own `Frame_Content_Size`, if its header survived.
fn damaged_len(frame: &[u8], listed: Option<(u64, u32)>) -> Option<u64> {
    if let Some((size, _)) = listed {
        return Some(size);
    }
    frames::header_of(frame, &data_limits())
        .ok()?
        .frame_content_size
        .filter(|&n| n > 0 && n % BLOCK as u64 == 0)
}

/// A frame read, and — if it is a data frame — decoded.
struct Prepared {
    frame: std::result::Result<Vec<u8>, String>,
    decoded: Option<std::result::Result<Vec<u8>, String>>,
}

/// Read `batch` in order, and decode its data frames in parallel.
fn prepare<S: Source + ?Sized>(
    src: &S,
    batch: &[Extent],
    dictionaries: &[Dictionary],
) -> std::collections::VecDeque<Prepared> {
    let frames: Vec<std::result::Result<Vec<u8>, String>> = batch
        .iter()
        .map(|x| {
            src.read_at(x.offset, x.size as usize)
                .map_err(|e| e.to_string())
        })
        .collect();
    // What pass 2 will treat as a data frame: anything not skippable, or
    // listed with content (a data frame whose magic was the damage).
    let is_data = |x: &Extent, f: &[u8]| {
        let magic = if f.len() >= 4 { le32(f) } else { 0 };
        !zalt_codec::magic::is_zstd_skippable(magic) || x.listed.is_some_and(|(n, _)| n > 0)
    };
    let decoded: Vec<Option<std::result::Result<Vec<u8>, String>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = batch
            .iter()
            .zip(&frames)
            .map(|(x, f)| match f {
                Ok(f) if is_data(x, f) => {
                    Some(scope.spawn(move || decode_data_frame(f, x.listed, dictionaries)))
                }
                _ => None,
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err("a decoding thread panicked".into()))
                })
            })
            .collect()
    });
    frames
        .into_iter()
        .zip(decoded)
        .map(|(frame, decoded)| Prepared { frame, decoded })
        .collect()
}

/// One frame pass 2 went through.
struct Walked {
    extent: Extent,
    stream_offset: u64,
    content_len: u64,
    checksum: u32,
    data: bool,
    good: bool,
    /// Whether a member starts exactly where this frame does.
    boundary: bool,
}

/// Rebuild `damaged` into `out`: every intact run of it written through
/// byte for byte, then a new Terminator, Central Index and seek table.
/// `options` sets how those are compressed.
///
/// Fails only when there is nothing to recover from — no Archive Header at
/// offset 0 — or the archive uses something this crate does not implement.
/// Damage after the header is what recovery is for: it costs members, and is
/// described in [`Recovered`], not returned as an error.
pub fn recover<S: Source + ?Sized, W: Write>(
    damaged: &S,
    out: W,
    options: WriterOptions,
) -> Result<(W, Recovered)> {
    recover_with(damaged, out, options, &RecoverOptions::default())
}

/// [`recover`](fn@recover), with [`RecoverOptions`].
pub fn recover_with<S: Source + ?Sized, W: Write>(
    damaged: &S,
    out: W,
    options: WriterOptions,
    how: &RecoverOptions,
) -> Result<(W, Recovered)> {
    let len = damaged.len();
    let max_header = ReadLimits::default().max_header_bytes;

    // The Archive Header, which must be first (§3.1).
    let head = damaged
        .read_at(0, 8)
        .map_err(|_| Error::Format("too short to hold an Archive Header".into()))?;
    if le32(&head) != MAGIC_HEADER {
        return Err(Error::Format("no Archive Header at offset 0 (§3.1)".into()));
    }
    let header_len = 8 + u64::from(le32(&head[4..]));
    let header_frame = damaged.read_at(0, usize::try_from(header_len).unwrap_or(usize::MAX))?;
    let header = Header::parse(skippable_payload(&header_frame, MAGIC_HEADER)?)?;
    // A Dictionary frame cannot be added after the fact, so the Terminator
    // this writes must not need one.
    let options = WriterOptions {
        dictionary: None,
        ..options
    };

    // Pass 1.
    let (extents, mut stopped) = extents(damaged, header_len);
    let (dictionaries, kept_dictionaries) = if header.flags & FLAG_DICT_REQUIRED != 0 {
        load_dictionaries(damaged, &extents)
    } else {
        (Vec::new(), Vec::new())
    };
    let index = extents.iter().rev().find_map(|x| {
        let frame = damaged.read_at(x.offset, x.size as usize).ok()?;
        let payload = skippable_payload(&frame, MAGIC_INDEX).ok()?;
        Index::parse(payload, &IndexLimits::default()).ok()
    });

    // Pass 2. Every member kept is hashed again as it is walked, with the
    // options' algorithm: the Archive Header does not say what the damaged
    // archive used, and its index — the only place that did — may be gone.
    // Frames are read in order and decoded a batch at a time, one per core;
    // the walk below then takes them in order, exactly as before.
    let threads = match options.threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    };
    let mut tar = TarWalk {
        algorithm: options.hash.algorithm(),
        threads,
        ..TarWalk::default()
    };
    let mut walked: Vec<Walked> = Vec::new();
    let mut stream = 0u64;
    let mut terminator = false;
    let mut damaged_frames = 0u64;
    let mut resynced = false;
    let mut ready: std::collections::VecDeque<Prepared> = std::collections::VecDeque::new();
    for (k, x) in extents.iter().enumerate() {
        if ready.is_empty() {
            ready = prepare(
                damaged,
                &extents[k..extents.len().min(k + threads)],
                &dictionaries,
            );
        }
        let Some(Prepared { frame, decoded }) = ready.pop_front() else {
            break;
        };
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                stopped = Some(format!("unreadable at offset {}: {e}", x.offset));
                break;
            }
        };
        let magic = if frame.len() >= 4 { le32(&frame) } else { 0 };
        // With a seek table, its sizes say what a frame is even when the
        // frame's own magic number was what got damaged.
        let listed_data = x.listed.is_some_and(|(n, _)| n > 0);
        if zalt_codec::magic::is_zstd_skippable(magic) && !listed_data {
            match magic {
                MAGIC_INDEX | MAGIC_SEEK_TABLE => break,
                // Kept if it loaded — it then sits before every data frame,
                // where kept frames that belong to no run are written.
                MAGIC_DICTIONARY => {
                    if kept_dictionaries.contains(&x.offset) {
                        walked.push(Walked {
                            extent: *x,
                            stream_offset: stream,
                            content_len: 0,
                            checksum: 0,
                            data: false,
                            good: true,
                            boundary: false,
                        });
                    }
                    continue;
                }
                MAGIC_HEADER => {
                    stopped = Some(format!("a second Archive Header at offset {}", x.offset));
                    break;
                }
                // Reserved and vendor frames: skipped, as §3.2 requires of a
                // scanning reader, and kept with the run they sit in.
                _ => {
                    walked.push(Walked {
                        extent: *x,
                        stream_offset: stream,
                        content_len: 0,
                        checksum: 0,
                        data: false,
                        good: true,
                        boundary: false,
                    });
                    continue;
                }
            }
        }

        let decoded = decoded.unwrap_or_else(|| decode_data_frame(&frame, x.listed, &dictionaries));
        let mut boundary = tar.at_boundary();
        // Lost the thread at damage, and the index knows where a member
        // starts here: take it up again, if the tar header agrees.
        if tar.broken.is_some() {
            if let (Some(index), Ok(content)) = (&index, &decoded) {
                // Entries are in stream order (§7.1), so this is a search, not
                // a scan per frame.
                let named = index
                    .entries
                    .binary_search_by_key(&stream, |e| e.header_offset)
                    .ok()
                    .map(|k| &index.entries[k]);
                let agrees = named.is_some_and(|e| {
                    matches!(tar::parse_member_prefix(content), Ok(Some(m)) if m.path == e.path)
                });
                if agrees {
                    tar.resync(stream, false);
                    boundary = true;
                    resynced = true;
                }
            }
        }
        // No index to ask: if asked to, guess that a frame starting with a
        // valid tar header starts a member.
        if tar.broken.is_some() && how.search {
            if let Ok(content) = &decoded {
                if matches!(tar::parse_member_prefix(content), Ok(Some(_))) {
                    tar.resync(stream, true);
                    boundary = true;
                }
            }
        }
        match decoded {
            Ok(content) => {
                let zeros = content.len() >= 2 * BLOCK
                    && content.len() % BLOCK == 0
                    && content.iter().all(|&b| b == 0);
                if boundary && zeros {
                    terminator = true;
                    break;
                }
                walked.push(Walked {
                    extent: *x,
                    stream_offset: stream,
                    content_len: content.len() as u64,
                    checksum: frames::seek_checksum(&content),
                    data: true,
                    good: true,
                    boundary,
                });
                tar.feed(&content, max_header);
                stream += content.len() as u64;
            }
            Err(why) => {
                damaged_frames += 1;
                let Some(n) = damaged_len(&frame, x.listed) else {
                    stopped = Some(format!(
                        "the data frame at offset {} is damaged ({why}), and how much of \
                         the tar stream it held cannot be told",
                        x.offset
                    ));
                    break;
                };
                walked.push(Walked {
                    extent: *x,
                    stream_offset: stream,
                    content_len: n,
                    checksum: 0,
                    data: true,
                    good: false,
                    boundary,
                });
                tar.gap(n, stream);
                stream += n;
            }
        }
    }
    if stopped.is_none() && !tar.at_boundary() {
        stopped = tar.broken.clone();
    }

    // Runs: from each boundary data frame to the next, the last one ending
    // where the walk did. Keep the runs whose frames all decoded and whose
    // members tile them exactly.
    let starts: Vec<usize> = (0..walked.len())
        .filter(|&i| walked[i].data && walked[i].boundary)
        .collect();
    let mut keep = vec![false; walked.len()];
    let mut kept_members: Vec<(usize, &Found)> = Vec::new();
    let mut lost: Vec<Vec<u8>> = Vec::new();
    let mut found = tar.members.iter().peekable();
    for (r, &first) in starts.iter().enumerate() {
        let last = starts.get(r + 1).copied().unwrap_or(walked.len());
        let begin = walked[first].stream_offset;
        let end = walked.get(last).map_or(stream, |w| w.stream_offset);
        let frames_good = walked[first..last].iter().all(|w| w.good);
        let mut members = Vec::new();
        while let Some(m) = found.next_if(|m| m.entry.header_offset < end) {
            if m.entry.header_offset >= begin {
                members.push(m);
            }
        }
        let mut cursor = begin;
        let mut tiles = true;
        for m in &members {
            tiles &= m.entry.header_offset == cursor && !m.damaged;
            cursor = m.end;
        }
        tiles &= cursor == end;
        // The last run is complete only if the walk ended between members.
        let complete = last < walked.len() || terminator || tar.at_boundary();
        if frames_good && tiles && complete && !members.is_empty() {
            keep[first..last].fill(true);
            kept_members.extend(members.into_iter().map(|m| (first, m)));
        } else {
            lost.extend(members.into_iter().map(|m| m.entry.path.clone()));
        }
    }
    lost.extend(found.map(|m| m.entry.path.clone()));
    // The walk can only name members whose headers it read. A surviving
    // index names the rest: everything it lists that is not being kept.
    if let Some(index) = &index {
        let kept: std::collections::HashSet<&[u8]> = kept_members
            .iter()
            .map(|(_, m)| m.entry.path.as_slice())
            .collect();
        let mut named: Vec<Vec<u8>> = index
            .entries
            .iter()
            .filter(|e| !kept.contains(e.path.as_slice()))
            .map(|e| e.path.clone())
            .collect();
        let listed: std::collections::HashSet<Vec<u8>> = named.iter().cloned().collect();
        for p in lost.drain(..) {
            if !listed.contains(&p) {
                named.push(p);
            }
        }
        lost = named;
    }
    // Vendor frames before the first data frame belong to no run: keep them.
    for (i, w) in walked.iter().enumerate() {
        if w.data {
            break;
        }
        keep[i] = true;
    }

    let kept_paths_searched: Vec<Vec<u8>> = kept_members
        .iter()
        .filter(|(_, m)| m.searched)
        .map(|(_, m)| m.entry.path.clone())
        .collect();

    // Write: the header, then every kept frame as it was, then the trailer.
    let mut out = out;
    out.write_all(&header_frame)
        .map_err(|e| Error::Io("writing the recovered archive".into(), e))?;
    let resume = Resume {
        seek: vec![SeekEntry {
            compressed_size: u32::try_from(header_len)
                .map_err(|_| Error::Refused("an Archive Header over 4 GiB".into()))?,
            decompressed_size: 0,
            checksum: Some(0),
        }],
        entries: Vec::new(),
        stream: 0,
        written: header_len,
        data_frames: 0,
        hash_algorithm: options.hash.algorithm(),
    };
    let mut writer = Writer::resume(out, options, resume)?;
    let mut kept_bytes = header_len;
    let mut members = kept_members.into_iter().peekable();
    for (i, w) in walked.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        if w.data && w.boundary {
            let shift = writer.stream_position() as i128 - w.stream_offset as i128;
            while let Some((_, m)) = members.next_if(|(run, _)| *run == i) {
                let mut e = m.entry.clone();
                e.header_offset = (e.header_offset as i128 + shift) as u64;
                e.data_offset = (e.data_offset as i128 + shift) as u64;
                writer.splice_entry(e);
            }
        }
        let frame = damaged.read_at(w.extent.offset, w.extent.size as usize)?;
        writer.splice_frame(&frame, w.content_len, w.checksum)?;
        kept_bytes += w.extent.size;
    }
    let found_by_search = kept_paths_searched;
    let (out, summary) = writer.finish()?;
    Ok((
        out,
        Recovered {
            entries: summary.entries,
            lost,
            damaged_frames,
            terminator_found: terminator,
            resynced_by_index: resynced,
            found_by_search,
            kept: kept_bytes,
            dropped: len - kept_bytes,
            stopped,
            summary,
        },
    ))
}
