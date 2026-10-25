//! Writing an archive (spec §3–§9, with §12's guidance as the defaults).
//!
//! ## Where entries go
//!
//! §5.3 decides how an entry may be split; §12.1 suggests the sizes, and the
//! defaults depart from it on purpose (below). An entry — its headers and
//! padded content — that is:
//!
//! - **below `small_file_threshold`** joins the current *solid group*, which
//!   is closed once the next entry would take it past `solid_target`. Files
//!   compress far better together, and extracting one still decodes at most
//!   one group.
//! - **up to `solid_target`** gets a frame of its own. §5.3: a writer should
//!   not split an entry that fits a solid frame. With the defaults, where the
//!   threshold *is* the group size, this only happens to an entry of exactly
//!   that size.
//! - **larger** is split into `large_chunk` (32 MiB) frames at 512-byte
//!   boundaries, headers in the first, so that reading part of a big file
//!   decodes only the chunks that hold it — and the chunks decode in parallel.
//!
//! ## Why the defaults are 8 MiB / 8 MiB, not §12.1's 256 KiB / 4 MiB
//!
//! Every frame boundary costs ratio: the next frame starts with no history
//! and fresh statistics. Measured on 2026-10-05 against `tar | zstd` of the
//! identical tar stream, at level 19:
//!
//! - **`crates/` (2.3 MiB): §12.1's policy was 1.52x the size; with medium
//!   files allowed into groups it is 1.01x.** Two near-identical 300 KB test
//!   fixtures sat in frames of their own, so neither could match the other.
//! - **A 146 MiB slice of the cargo registry: 1.32x at 4 MiB groups, 1.22x at
//!   8 MiB, 1.12x at 16 MiB, 1.07x at 32 MiB.** No knee at the 8 MiB match
//!   window: each doubling roughly halves the overhead and doubles what a
//!   one-file extraction decodes (3.6 → 7.9 → 15.6 → 30.4 MiB on average).
//!
//! 8 MiB is a middle setting, not an optimum; there is none, only the trade.
//! At the default level 3 it already writes archives no larger than
//! `tar | zstd -3` (0.98x on the registry slice). Extraction time was not
//! measured, only bytes decoded; a reader that wants faster single-file
//! access writes smaller groups through [`FramePolicy`].
//!
//! ## Parallel, and still deterministic
//!
//! Frames are independent, so they are compressed across cores in batches
//! (§12.3) and written in order. Each frame's bytes depend only on its own
//! content and the level, never on which thread compressed it or what else was
//! in the batch — so the thread count cannot change the output, and §12.4's
//! reproducibility holds at any parallelism. A test asserts exactly that.
//!
//! ## Dictionaries
//!
//! With [`WriterOptions::dictionary`], the dictionary goes in a Dictionary
//! frame straight after the header (§6), `DICT_REQUIRED` is set, and every
//! data frame — the Terminator too — is compressed against it. So the flag is
//! always true of the archive (§4.2), and an appender finds the dictionary it
//! should go on using. The Central Index never uses it (§6).
//!
//! ## Memory
//!
//! A batch holds at most `threads` payloads and at most [`BATCH_BYTES`] in
//! total, so a writer's peak is bounded by those, not by the archive. Large
//! files are read a chunk at a time and never held whole.

use std::collections::HashSet;
use std::io::{Read, Write};

use crate::dict::Dictionary;
use crate::hash::{Hash, Hasher};
use crate::index::{Entry, Index, Kind};
use crate::layout::{
    FLAG_DICT_REQUIRED, Header, MAGIC_DICTIONARY, MAGIC_HEADER, MAGIC_INDEX, SeekEntry,
    TAG_CREATOR, encode_seek_table, skippable,
};
use crate::meta;
use crate::tar::{self, BLOCK, Extras, pad};
use crate::{Error, Result, frames};

/// The most uncompressed bytes a batch holds before it is compressed.
pub const BATCH_BYTES: u64 = 256 << 20;

/// Where frame boundaries go (§12.1's settings). The defaults, and why they
/// differ from §12.1's suggestion, are in the module notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePolicy {
    /// Entries below this go into solid groups.
    pub small_file_threshold: u64,
    /// A solid group closes before it would pass this.
    pub solid_target: u64,
    /// Large entries are split into frames of this many bytes.
    pub large_chunk: u64,
}

impl Default for FramePolicy {
    fn default() -> Self {
        Self {
            small_file_threshold: 8 << 20,
            solid_target: 8 << 20,
            large_chunk: 32 << 20,
        }
    }
}

impl FramePolicy {
    fn validate(&self) -> Result<()> {
        let block = BLOCK as u64;
        let ok = self.large_chunk % block == 0
            && self.large_chunk >= block
            && self.large_chunk <= frames::MAX_FRAME_CONTENT
            && self.solid_target <= frames::MAX_FRAME_CONTENT
            && self.small_file_threshold <= self.solid_target;
        if ok {
            Ok(())
        } else {
            Err(Error::Refused(
                "frame policy: chunks must be whole 512-byte blocks, frames at most 1 GiB, \
                 and the small-file threshold no larger than the solid target"
                    .into(),
            ))
        }
    }
}

/// How an archive is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterOptions {
    /// zalt compression level for every frame, index included.
    pub level: i32,
    pub policy: FramePolicy,
    /// Worker threads; 0 means one per core.
    pub threads: usize,
    /// The `Creator` header TLV. `Created` is never written: §12.4 lists its
    /// absence as a condition of reproducible output.
    pub creator: Option<Vec<u8>>,
    /// The hash each member's content gets (§7.3). BLAKE3 by default; see
    /// [`crate::hash`]. Ignored when appending: an index has one
    /// `Hash_Algorithm`, so an appended member gets the archive's own.
    pub hash: Hash,
    /// Compress every data frame against this dictionary (§6). The archive is
    /// then no longer readable by `zstd -dc` alone. When appending, it must be
    /// one the archive already holds — `None` keeps using the archive's — and
    /// recovery ignores it.
    pub dictionary: Option<Dictionary>,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            level: zalt_codec::encode::level::LEVEL_DEFAULT,
            policy: FramePolicy::default(),
            threads: 0,
            creator: Some(format!("zalt {}", env!("CARGO_PKG_VERSION")).into_bytes()),
            hash: Hash::default(),
            dictionary: None,
        }
    }
}

/// What a finished archive turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub entries: u64,
    /// Length of the tar stream, Terminator included.
    pub tar_stream_size: u64,
    pub archive_size: u64,
    /// Data frames, Terminator included.
    pub data_frames: u64,
}

/// Writes one archive to `W`.
pub struct Writer<W: Write> {
    out: W,
    options: WriterOptions,
    threads: usize,
    /// Bytes written so far: the file offset of the next frame.
    written: u64,
    /// Stream offset of the next byte of tar stream to be produced.
    stream: u64,
    seek: Vec<SeekEntry>,
    entries: Vec<Entry>,
    /// The open solid group's tar bytes.
    group: Vec<u8>,
    /// Payloads awaiting compression, in order.
    batch: Vec<Vec<u8>>,
    batch_bytes: u64,
    data_frames: u64,
    /// Every path in the archive so far, to refuse a second member with one.
    paths: HashSet<Vec<u8>>,
    /// The index's `Hash_Algorithm`: the options' for a new archive, an
    /// appended archive's own otherwise.
    hash_algorithm: u8,
}

/// Where a resumed [`Writer`] picks up: an archive up to, and not including,
/// its Terminator. Built by [`crate::append`](fn@crate::append) from an archive's index, and by
/// [`crate::recover`](fn@crate::recover) from a walk of a damaged one.
#[derive(Debug, Clone)]
pub(crate) struct Resume {
    /// Seek table entries for every frame before the Terminator, in order —
    /// the Archive Header first.
    pub seek: Vec<SeekEntry>,
    /// Index records for every member before the Terminator.
    pub entries: Vec<Entry>,
    /// Stream offset where the next data frame starts.
    pub stream: u64,
    /// File offset where the next frame starts: the length of what is kept.
    pub written: u64,
    /// Data frames kept.
    pub data_frames: u64,
    pub hash_algorithm: u8,
}

impl<W: Write> Writer<W> {
    /// Start an archive, writing its header.
    pub fn new(out: W, options: WriterOptions) -> Result<Self> {
        options.policy.validate()?;
        let threads = match options.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        let mut w = Self {
            out,
            options,
            threads,
            written: 0,
            stream: 0,
            seek: Vec::new(),
            entries: Vec::new(),
            group: Vec::new(),
            batch: Vec::new(),
            batch_bytes: 0,
            data_frames: 0,
            paths: HashSet::new(),
            hash_algorithm: 0,
        };
        w.hash_algorithm = w.options.hash.algorithm();
        let mut header = Header::default();
        if let Some(creator) = &w.options.creator {
            header.tlvs.push((TAG_CREATOR, creator.clone()));
        }
        if w.options.dictionary.is_some() {
            header.flags |= FLAG_DICT_REQUIRED;
        }
        let frame = skippable(MAGIC_HEADER, &header.encode()?)?;
        w.emit_skippable(&frame, "Archive Header")?;
        // §6: before the first data frame, so a streaming reader has it first.
        if let Some(d) = &w.options.dictionary {
            let frame = skippable(MAGIC_DICTIONARY, d.raw())?;
            w.emit_skippable(&frame, "Dictionary")?;
        }
        Ok(w)
    }

    /// Continue an archive whose first `resume.written` bytes are already in
    /// `out`, ending just before where its Terminator was. The Archive Header
    /// is not rewritten, so `options.creator` is not used.
    pub(crate) fn resume(out: W, options: WriterOptions, resume: Resume) -> Result<Self> {
        options.policy.validate()?;
        let threads = match options.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        let paths = resume.entries.iter().map(|e| e.path.clone()).collect();
        Ok(Self {
            out,
            options,
            threads,
            written: resume.written,
            stream: resume.stream,
            seek: resume.seek,
            entries: resume.entries,
            group: Vec::new(),
            batch: Vec::new(),
            batch_bytes: 0,
            data_frames: resume.data_frames,
            paths,
            hash_algorithm: resume.hash_algorithm,
        })
    }

    /// Where the next data frame starts in the tar stream.
    pub(crate) fn stream_position(&self) -> u64 {
        self.stream + self.group.len() as u64
    }

    /// Write `frame` exactly as given — recovery's way of keeping an archive's
    /// undamaged frames byte for byte. `content_len` and `checksum` describe
    /// its decoded content for the seek table; a skippable frame has
    /// `content_len` 0 and is listed as §8.2 requires.
    pub(crate) fn splice_frame(
        &mut self,
        frame: &[u8],
        content_len: u64,
        checksum: u32,
    ) -> Result<()> {
        self.close_group()?;
        self.compress_batch()?;
        let size =
            |n: u64| u32::try_from(n).map_err(|_| Error::Refused("a frame over 4 GiB".into()));
        let entry = SeekEntry {
            compressed_size: size(frame.len() as u64)?,
            decompressed_size: size(content_len)?,
            checksum: Some(if content_len == 0 { 0 } else { checksum }),
        };
        self.write(frame, "a kept frame")?;
        self.seek.push(entry);
        if content_len > 0 {
            self.stream += content_len;
            self.data_frames += 1;
        }
        Ok(())
    }

    /// Record a member held by frames [`Writer::splice_frame`] writes. Its
    /// offsets are the caller's to have set. A path seen before is recorded
    /// again rather than refused: recovery reproduces what an archive holds,
    /// and a member left out of the index would break §7's one record per
    /// member.
    pub(crate) fn splice_entry(&mut self, entry: Entry) {
        self.paths.insert(entry.path.clone());
        self.entries.push(entry);
    }

    /// Add one member. A regular file's `entry.size` bytes are read from    /// Add one member. A regular file's `entry.size` bytes are read from
    /// `content`; for every other kind it is not read.
    ///
    /// `header_offset` and `data_offset` are the writer's to fill and are
    /// overwritten.
    pub fn add(&mut self, entry: Entry, content: &mut dyn Read) -> Result<()> {
        self.add_with(entry, &Extras::default(), content)
    }

    /// [`Writer::add`], with the metadata that lives only in the tar stream.
    ///
    /// `entry.xattrs` — the index's names and sizes — is rebuilt from
    /// `extras`, so the index can never describe xattrs the tar stream does
    /// not hold. Flags outside [`meta::RECORDED_FLAGS`] are dropped (§9.3:
    /// `UF_COMPRESSED` must be), and empty names and flags are recorded as
    /// absent, which is how the tar stream will read back.
    pub fn add_with(
        &mut self,
        mut entry: Entry,
        extras: &Extras,
        content: &mut dyn Read,
    ) -> Result<()> {
        if !entry.kind.has_content() {
            entry.size = 0;
        }
        entry.xattrs = extras
            .xattrs
            .iter()
            .map(|(name, value)| (name.clone(), value.len() as u64))
            .collect();
        entry.fflags = entry
            .fflags
            .map(|f| f & meta::RECORDED_FLAGS)
            .filter(|&f| f != 0);
        if entry
            .names
            .as_ref()
            .is_some_and(|(u, g)| u.is_empty() && g.is_empty())
        {
            entry.names = None;
        }
        if !matches!(entry.kind, Kind::CharDevice | Kind::BlockDevice) {
            entry.device = None;
        }
        if let Some(acl) = &extras.acl {
            meta::parse_acl(acl)?;
        }
        // One member per path: the index looks entries up by path, and §10
        // has no replacing. Checked before anything is written, so a refused
        // member leaves the archive as it was.
        if self.paths.contains(&entry.path) {
            return Err(Error::Refused(format!(
                "{} is already in the archive; members cannot be replaced (§10)",
                entry.path_lossy()
            )));
        }
        if matches!(entry.kind, Kind::Symlink | Kind::HardLink) && entry.link.is_none() {
            return Err(Error::Refused(format!(
                "{} has no link target",
                entry.path_lossy()
            )));
        }
        // The hash covers the content (§7.2), so only members that have
        // some get one — an empty file included, whose hash is well defined.
        // Whatever the caller put in `entry.hash` is the writer's to replace.
        entry.hash = None;
        let mut hasher = if entry.kind.has_content() {
            Hasher::new(self.hash_algorithm, self.threads)
        } else {
            None
        };
        let mut headers = Vec::with_capacity(BLOCK * 2);
        let header_len = tar::write_headers(&mut headers, &entry, extras)?;
        self.paths.insert(entry.path.clone());
        let total = header_len + pad(entry.size);
        let policy = self.options.policy;

        if total < policy.small_file_threshold {
            if !self.group.is_empty() && self.group.len() as u64 + total > policy.solid_target {
                self.close_group()?;
            }
            entry.header_offset = self.stream + self.group.len() as u64;
            entry.data_offset = entry.header_offset + header_len;
            self.group.extend_from_slice(&headers);
            let name = entry.path_lossy().into_owned();
            read_content(content, &mut self.group, entry.size, &name, hasher.as_mut())?;
            entry.hash = hasher.map(|h| h.finish());
            self.entries.push(entry);
            return Ok(());
        }

        self.close_group()?;
        entry.header_offset = self.stream;
        entry.data_offset = entry.header_offset + header_len;
        let size = entry.size;
        let name = entry.path_lossy().into_owned();
        self.entries.push(entry);

        if total <= policy.solid_target {
            let mut payload = headers;
            read_content(content, &mut payload, size, &name, hasher.as_mut())?;
            self.set_last_hash(hasher);
            return self.queue(payload);
        }

        // Large: chunk the member — headers, content, padding — into frames of
        // exactly `large_chunk` bytes, the last taking what remains.
        let chunk = policy.large_chunk as usize;
        let mut payload = headers;
        let mut remaining = size;
        loop {
            let room = (chunk - payload.len()) as u64;
            let take = room.min(remaining);
            read_exact_into(content, &mut payload, take, &name, hasher.as_mut())?;
            remaining -= take;
            if remaining == 0 {
                payload.resize(pad(payload.len() as u64) as usize, 0);
                self.set_last_hash(hasher);
                return self.queue(payload);
            }
            self.queue(std::mem::replace(&mut payload, Vec::with_capacity(chunk)))?;
        }
    }

    /// Write the Terminator, the Central Index and the seek table, and return
    /// the underlying writer.
    pub fn finish(mut self) -> Result<(W, Summary)> {
        self.close_group()?;
        // §5.4: exactly 1024 zero bytes, in a frame of their own, so an
        // appender can replace it without touching anything before it.
        self.queue(vec![0u8; 2 * BLOCK])?;
        self.compress_batch()?;

        let index = Index {
            hash_algorithm: self.hash_algorithm,
            tar_stream_size: self.stream,
            entries: std::mem::take(&mut self.entries),
        };
        let entries = index.entries.len() as u64;
        let frame = skippable(MAGIC_INDEX, &index.encode(self.options.level)?)?;
        self.emit_skippable(&frame, "Central Index")?;

        let table = encode_seek_table(&self.seek)?;
        self.write(&table, "seek table")?;
        self.out
            .flush()
            .map_err(|e| Error::Io("flushing the archive".into(), e))?;
        let summary = Summary {
            entries,
            tar_stream_size: self.stream,
            archive_size: self.written,
            data_frames: self.data_frames,
        };
        Ok((self.out, summary))
    }

    /// Record the hash of the member just pushed, now its content is read.
    fn set_last_hash(&mut self, hasher: Option<Hasher>) {
        if let Some(e) = self.entries.last_mut() {
            e.hash = hasher.map(|h| h.finish());
        }
    }

    fn close_group(&mut self) -> Result<()> {
        if self.group.is_empty() {
            return Ok(());
        }
        let group = std::mem::take(&mut self.group);
        self.queue(group)
    }

    /// Hand a frame's worth of tar stream to the batch, advancing the stream.
    fn queue(&mut self, payload: Vec<u8>) -> Result<()> {
        debug_assert_eq!(
            payload.len() % BLOCK,
            0,
            "frames are whole tar blocks (§5.2)"
        );
        self.stream += payload.len() as u64;
        self.batch_bytes += payload.len() as u64;
        self.batch.push(payload);
        if self.batch.len() >= self.threads || self.batch_bytes >= BATCH_BYTES {
            self.compress_batch()?;
        }
        Ok(())
    }

    /// Compress the batch across threads and write the frames in order.
    fn compress_batch(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.batch);
        self.batch_bytes = 0;
        let level = self.options.level;
        let dictionary = self.options.dictionary.as_ref();
        let compressed: Vec<Result<Vec<u8>>> = if batch.len() <= 1 {
            batch
                .iter()
                .map(|p| frames::encode(p, level, dictionary))
                .collect()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|p| scope.spawn(move || frames::encode(p, level, dictionary)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or_else(|_| {
                            Err(Error::Refused("a compression thread panicked".into()))
                        })
                    })
                    .collect()
            })
        };
        for (payload, frame) in batch.iter().zip(compressed) {
            let frame = frame?;
            let entry = SeekEntry {
                compressed_size: u32::try_from(frame.len())
                    .map_err(|_| Error::Refused("a compressed frame over 4 GiB".into()))?,
                decompressed_size: payload.len() as u32,
                checksum: Some(frames::seek_checksum(payload)),
            };
            self.write(&frame, "a data frame")?;
            self.seek.push(entry);
            self.data_frames += 1;
        }
        Ok(())
    }

    fn emit_skippable(&mut self, frame: &[u8], what: &str) -> Result<()> {
        self.write(frame, what)?;
        // §8.2: a skippable frame is listed with no content and checksum 0.
        self.seek.push(SeekEntry {
            compressed_size: u32::try_from(frame.len())
                .map_err(|_| Error::Refused(format!("{what} over 4 GiB")))?,
            decompressed_size: 0,
            checksum: Some(0),
        });
        Ok(())
    }

    fn write(&mut self, bytes: &[u8], what: &str) -> Result<()> {
        self.out
            .write_all(bytes)
            .map_err(|e| Error::Io(format!("writing {what}"), e))?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

/// Read a file's content into `out`, padded to whole blocks.
fn read_content(
    content: &mut dyn Read,
    out: &mut Vec<u8>,
    size: u64,
    name: &str,
    hasher: Option<&mut Hasher>,
) -> Result<()> {
    read_exact_into(content, out, size, name, hasher)?;
    out.resize(pad(out.len() as u64) as usize, 0);
    Ok(())
}

/// Read exactly `n` bytes, hashing them as they arrive. A file that shrank
/// after its size was taken is an error, not a short member: the header
/// already promised `n`.
fn read_exact_into(
    content: &mut dyn Read,
    out: &mut Vec<u8>,
    n: u64,
    name: &str,
    hasher: Option<&mut Hasher>,
) -> Result<()> {
    let before = out.len();
    content
        .take(n)
        .read_to_end(out)
        .map_err(|e| Error::Io(format!("reading {name}"), e))?;
    if (out.len() - before) as u64 != n {
        return Err(Error::Io(
            format!("reading {name}"),
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file is shorter than when its size was taken",
            ),
        ));
    }
    if let Some(h) = hasher {
        h.update(&out[before..]);
    }
    Ok(())
}
