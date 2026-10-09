//! Reading an archive: the R1 class of §14.2 — indexed open (§11.1),
//! extracting entries (§11.2), and the §13 rules that make both safe on
//! untrusted input.
//!
//! ## Nothing is trusted that can be checked
//!
//! §11.1 lists the checks for an indexed open; this does all of them, and the
//! ones the spec states only as layout (§3.1) besides: the frames must appear
//! in the order the spec lists them, skippable ones must claim no content, data
//! frames must be whole tar blocks and at most 1 GiB, the index's stream length
//! must be the data frames' total, and every entry must lie before the
//! Terminator. A reader that skipped any of these would be reading an archive
//! the spec does not describe, and acting on offsets it had no reason to
//! believe.
//!
//! Extracting an entry then trusts the index only to say *where to look*: the
//! frames are decoded and checked against the seek table's checksums, and the
//! tar headers found there are compared with the index record before a byte of
//! content is returned. §7.5 makes the tar stream authoritative; a disagreement
//! is an error, not a choice.

use std::io::Write;

use crate::index::{Entry, Index, IndexLimits};
use crate::layout::{
    FLAG_DICT_REQUIRED, Header, MAGIC_DICTIONARY, MAGIC_HEADER, MAGIC_INDEX, MAGIC_SEEK_TABLE,
    SeekEntry, is_zstd_frame, le32, read_seek_table, skippable_payload,
};
use crate::meta;
use crate::tar::{self, BLOCK, pad};
use crate::{Error, Result, format_err, frames};

/// Random access to an archive's bytes.
///
/// `Sync`, because extraction reads several frames at once from worker
/// threads.
pub trait Source: Sync {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Source for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let start =
            usize::try_from(offset).map_err(|_| Error::Format("offset past the file".into()))?;
        self.get(start..start.saturating_add(len))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| Error::Format("read past the end of the archive".into()))
    }
}

impl Source for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.as_slice().read_at(offset, len)
    }
}

impl Source for std::fs::File {
    fn len(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; len];
        self.read_exact_at(&mut buf, offset)
            .map_err(|e| Error::Io(format!("reading {len} bytes at {offset}"), e))?;
        Ok(buf)
    }
}

/// Bounds on what an archive may make the reader do.
#[derive(Debug, Clone, Copy)]
pub struct ReadLimits {
    pub index: IndexLimits,
    /// Seek table entries.
    pub max_frames: u64,
    /// Frames decoded at once while extracting.
    pub threads: usize,
    /// Bytes of one member's headers. Its pax records — xattr values above all
    /// — are read whole, so this is §13's bound on xattr size; the default
    /// leaves room for a large resource fork.
    pub max_header_bytes: u64,
    /// Most bytes of decoded frames kept for reuse. Members that share a
    /// solid group share its decode through this, and sequential reads
    /// decode a window of frames ahead, in parallel, into it.
    pub cache_bytes: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            index: IndexLimits::default(),
            max_frames: 1 << 24,
            threads: 0,
            max_header_bytes: 64 << 20,
            cache_bytes: 64 << 20,
        }
    }
}

/// One frame, located.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub file_offset: u64,
    pub compressed_size: u64,
    /// Where its content starts in the tar stream; for a skippable frame, the
    /// offset it sits at.
    pub stream_offset: u64,
    pub decompressed_size: u64,
    pub checksum: Option<u32>,
    pub magic: u32,
}

impl Frame {
    pub fn is_data(&self) -> bool {
        is_zstd_frame(self.magic)
    }
}

/// An archive opened by its index.
pub struct Archive<S: Source + ?Sized> {
    source: Box<S>,
    header: Header,
    index: Index,
    frames: Vec<Frame>,
    /// Indices into `frames` of the data frames, Terminator last.
    data: Vec<usize>,
    threads: usize,
    max_header_bytes: u64,
    cache_bytes: u64,
    /// Content bytes decoded so far, by every operation: what reading costs,
    /// as a number a test can hold to account.
    decoded: std::sync::atomic::AtomicU64,
    /// Recently decoded frames. A solid group holds many members, and reading
    /// each of them — its headers, then its content — would otherwise decode
    /// the whole group again every time: members × group size, which on a
    /// real tree is hundreds of times the archive.
    cache: std::sync::Mutex<FrameCache>,
}

/// Least recently used first, at most `limit` bytes — bounded, so memory
/// does not grow with the archive.
#[derive(Default)]
struct FrameCache {
    frames: std::collections::VecDeque<(usize, std::sync::Arc<Vec<u8>>)>,
    bytes: u64,
    limit: u64,
}

impl FrameCache {
    fn get(&mut self, i: usize) -> Option<std::sync::Arc<Vec<u8>>> {
        let at = self.frames.iter().position(|(j, _)| *j == i)?;
        let hit = self.frames.remove(at)?;
        let content = hit.1.clone();
        self.frames.push_back(hit);
        Some(content)
    }

    fn put(&mut self, i: usize, content: std::sync::Arc<Vec<u8>>) {
        let size = content.len() as u64;
        if size > self.limit || self.frames.iter().any(|(j, _)| *j == i) {
            return;
        }
        self.frames.push_back((i, content));
        self.bytes += size;
        while self.bytes > self.limit {
            let Some((_, old)) = self.frames.pop_front() else {
                break;
            };
            self.bytes -= old.len() as u64;
        }
    }
}

impl<S: Source + ?Sized> std::fmt::Debug for Archive<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("entries", &self.index.entries.len())
            .field("frames", &self.frames.len())
            .finish()
    }
}

impl<S: Source + ?Sized> Archive<S> {
    /// Open an archive by its trailer (§11.1).
    pub fn open(source: Box<S>, limits: ReadLimits) -> Result<Self> {
        let file_len = source.len();

        // Steps 1–4: the seek table, checked to cover the file exactly.
        let (seek, _) =
            read_seek_table(file_len, |at, n| source.read_at(at, n), limits.max_frames)?;
        let frames = locate(&*source, &seek)?;
        let data = check_layout(&frames)?;

        // Step 5: the header.
        let first = &frames[0];
        let header_frame = source.read_at(first.file_offset, first.compressed_size as usize)?;
        let header = Header::parse(skippable_payload(&header_frame, MAGIC_HEADER)?)?;
        if header.flags & FLAG_DICT_REQUIRED != 0 {
            return Err(Error::Unsupported("dictionary archives (§6)".into()));
        }

        // Step 6: the index, the last listed frame.
        let last = frames[frames.len() - 1];
        let index_frame = source.read_at(last.file_offset, last.compressed_size as usize)?;
        let index = Index::parse(skippable_payload(&index_frame, MAGIC_INDEX)?, &limits.index)?;

        let stream: u64 = data.iter().map(|&i| frames[i].decompressed_size).sum();
        if index.tar_stream_size != stream {
            return format_err(format!(
                "Tar_Stream_Size is {} but the data frames hold {stream} bytes",
                index.tar_stream_size
            ));
        }
        let terminator = frames[*data.last().expect("check_layout requires a Terminator")];
        if let Some(e) = index
            .entries
            .iter()
            .find(|e| e.data_offset + pad(e.size) > terminator.stream_offset)
        {
            return format_err(format!("{} runs into the Terminator", e.path_lossy()));
        }

        let threads = match limits.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        Ok(Self {
            source,
            header,
            index,
            frames,
            data,
            threads,
            max_header_bytes: limits.max_header_bytes,
            cache_bytes: limits.cache_bytes,
            decoded: std::sync::atomic::AtomicU64::new(0),
            cache: std::sync::Mutex::new(FrameCache {
                limit: limits.cache_bytes,
                ..FrameCache::default()
            }),
        })
    }

    /// The index's `Hash_Algorithm` (§7.3).
    pub fn hash_algorithm(&self) -> u8 {
        self.index.hash_algorithm
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The members, in tar-stream order.
    pub fn entries(&self) -> &[Entry] {
        &self.index.entries
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn tar_stream_size(&self) -> u64 {
        self.index.tar_stream_size
    }

    /// The member at `path`, if there is one.
    pub fn find(&self, path: &[u8]) -> Option<&Entry> {
        self.index.entries.iter().find(|e| e.path == path)
    }

    /// Decode one data frame, checked against the seek table.
    /// Content bytes this archive has decoded so far, across every
    /// operation. Decoding is where reading spends its time, so this is the
    /// deterministic measure of what an operation costs.
    pub fn bytes_decoded(&self) -> u64 {
        self.decoded.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn cached(&self, i: usize) -> Option<std::sync::Arc<Vec<u8>>> {
        self.cache.lock().ok()?.get(i)
    }

    fn remember(&self, i: usize, content: &std::sync::Arc<Vec<u8>>) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(i, content.clone());
        }
    }

    /// Data frame `i`'s content, decoded and checked — or reused, if it was
    /// decoded recently.
    pub(crate) fn decode_frame(&self, i: usize) -> Result<std::sync::Arc<Vec<u8>>> {
        if let Some(content) = self.cached(i) {
            return Ok(content);
        }
        let content = std::sync::Arc::new(self.decode_uncached(i)?);
        self.remember(i, &content);
        Ok(content)
    }

    fn decode_uncached(&self, i: usize) -> Result<Vec<u8>> {
        let f = self.frames[i];
        self.decoded
            .fetch_add(f.decompressed_size, std::sync::atomic::Ordering::Relaxed);
        let bytes = self
            .source
            .read_at(f.file_offset, f.compressed_size as usize)?;
        let content = frames::decode_exactly(&bytes, f.decompressed_size)?;
        // §8.2: data frames' checksums are verified; zero-size entries' never.
        if let Some(sum) = f.checksum
            && frames::seek_checksum(&content) != sum
        {
            return format_err(format!("frame {i} does not match its seek table checksum"));
        }
        Ok(content)
    }

    /// Decode several data frames at once, `self.threads` at a time.
    /// Several frames' content, in order: reused where cached, the rest
    /// decoded in parallel, a batch of `threads` at a time.
    fn decode_frames(&self, which: &[usize]) -> Result<Vec<std::sync::Arc<Vec<u8>>>> {
        let mut out: Vec<Option<std::sync::Arc<Vec<u8>>>> =
            which.iter().map(|&i| self.cached(i)).collect();
        let missing: Vec<usize> = (0..which.len()).filter(|&k| out[k].is_none()).collect();
        for batch in missing.chunks(self.threads.max(1)) {
            let decoded: Vec<Result<Vec<u8>>> = if batch.len() == 1 {
                vec![self.decode_uncached(which[batch[0]])]
            } else {
                std::thread::scope(|scope| {
                    let handles: Vec<_> = batch
                        .iter()
                        .map(|&k| scope.spawn(move || self.decode_uncached(which[k])))
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().unwrap_or_else(|_| {
                                Err(Error::Refused("a decoding thread panicked".into()))
                            })
                        })
                        .collect()
                })
            };
            for (&k, content) in batch.iter().zip(decoded) {
                let content = std::sync::Arc::new(content?);
                self.remember(which[k], &content);
                out[k] = Some(content);
            }
        }
        Ok(out
            .into_iter()
            .map(|c| c.expect("every frame filled"))
            .collect())
    }

    /// The data frames whose content intersects `[start, end)` of the stream.
    fn frames_for(&self, start: u64, end: u64) -> Vec<usize> {
        // Data frames are in stream order; find the first that ends past
        // `start` and take frames until one starts at or past `end`.
        let first = self.data.partition_point(|&i| {
            let f = &self.frames[i];
            f.stream_offset + f.decompressed_size <= start
        });
        self.data[first..]
            .iter()
            .copied()
            .take_while(|&i| self.frames[i].stream_offset < end)
            .collect()
    }

    /// Feed `[start, end)` of the tar stream to `sink`, a batch of frames at a
    /// time, so a large range is never held whole.
    fn stream_range(
        &self,
        start: u64,
        end: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if end > self.index.tar_stream_size || start > end {
            return format_err("range outside the tar stream");
        }
        let needed = self.frames_for(start, end);
        for batch in needed.chunks(self.threads.max(1)) {
            for (&i, content) in batch.iter().zip(self.decode_frames(batch)?) {
                let f = self.frames[i];
                let from = start.saturating_sub(f.stream_offset) as usize;
                let to =
                    (end.min(f.stream_offset + f.decompressed_size) - f.stream_offset) as usize;
                sink(&content[from..to])?;
            }
        }
        Ok(())
    }

    /// `[start, end)` of the tar stream.
    pub fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity((end.saturating_sub(start)) as usize);
        self.stream_range(start, end, &mut |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(out)
    }

    /// An entry's tar headers — the authoritative record (§7.5), and the only
    /// one holding xattr values and ACLs — after checking that they agree
    /// with its index record.
    pub fn member(&self, e: &Entry) -> Result<tar::Member> {
        self.check_headers(e)
    }

    /// Parse an entry's tar headers and compare them with its index record.
    fn check_headers(&self, e: &Entry) -> Result<tar::Member> {
        let header_bytes = e.data_offset.saturating_sub(e.header_offset);
        if header_bytes > self.max_header_bytes {
            return Err(Error::Refused(format!(
                "{}: {header_bytes} bytes of tar headers, over the limit of {}",
                e.path_lossy(),
                self.max_header_bytes
            )));
        }
        let headers = self.read_range(e.header_offset, e.data_offset)?;
        let m = tar::parse_member(&headers)?;
        let disagree = |what: &str| {
            Err(Error::Inconsistent(format!(
                "{}: the index and the tar header disagree about {what}",
                e.path_lossy()
            )))
        };
        if m.header_len != e.data_offset - e.header_offset {
            return disagree("where the content starts");
        }
        if m.path != e.path {
            return disagree("the path");
        }
        if m.kind != e.kind {
            return disagree("the type");
        }
        if m.size != e.size {
            return disagree("the size");
        }
        if m.link != e.link {
            return disagree("the link target");
        }
        // The metadata the index summarises, so that listing by the index
        // tells the truth about it. Each field is optional (§7.2) — the index
        // is a cache (§7.5), and a writer may leave any of them out — so only
        // a field the index *has* must match; one it omits contradicts
        // nothing. Flags compare as the bits both sides can spell; xattrs by
        // name and size, in order.
        if e.btime.is_some() && m.btime != e.btime {
            return disagree("the birth time");
        }
        if e.names.is_some() && m.names != e.names {
            return disagree("the owner names");
        }
        let spelled = |f: Option<u64>| f.unwrap_or(0) & meta::RECORDED_FLAGS;
        if e.fflags.is_some() && spelled(m.fflags) != spelled(e.fflags) {
            return disagree("the file flags");
        }
        if e.device.is_some() && m.device != e.device {
            return disagree("the device numbers");
        }
        let sizes = m.xattrs.iter().map(|(n, v)| (n.as_slice(), v.len() as u64));
        if !e.xattrs.is_empty() && !sizes.eq(e.xattrs.iter().map(|(n, s)| (n.as_slice(), *s))) {
            return disagree("the extended attributes");
        }
        Ok(m)
    }

    /// Write an entry's content to `out` (§11.2), after checking its headers.
    /// Returns the tar header's view of the member.
    pub fn copy_entry(&self, e: &Entry, out: &mut dyn Write) -> Result<tar::Member> {
        let member = self.check_headers(e)?;
        if e.size > 0 {
            self.stream_range(e.data_offset, e.data_offset + e.size, &mut |chunk| {
                out.write_all(chunk)
                    .map_err(|err| Error::Io(format!("writing {}", e.path_lossy()), err))
            })?;
        }
        Ok(member)
    }

    /// An entry's content.
    pub fn entry_bytes(&self, e: &Entry) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(e.size as usize);
        self.copy_entry(e, &mut out)?;
        Ok(out)
    }

    /// Decode every frame and check the archive end to end.
    ///
    /// Beyond what [`Archive::open`] checks: every data frame decodes and
    /// matches its checksum; the Terminator is all zeros; every member's
    /// headers agree with its index record; and the members **tile the tar
    /// stream** — each starts where the previous one's padded content ends, and
    /// the last ends at the Terminator. That last property is what makes the
    /// index a complete account of the stream: a member present in the tar but
    /// missing from the index would leave a gap, and be caught here, where a
    /// stock `tar -x` would extract it without the index ever mentioning it.
    pub fn verify(&self) -> Result<()> {
        // One pass in stream order: a window of frames decoded in parallel,
        // then the headers of every member that window completes, checked
        // while it is still in the cache. Decoding everything first and
        // checking afterwards decoded every frame twice — the second time on
        // one thread — once an archive outgrew the cache.
        let mut entries = self.index.entries.iter().peekable();
        let mut expected = 0u64;
        let mut covered = 0u64;
        while covered < self.index.tar_stream_size {
            covered = self.prefetch(covered)?;
            while let Some(e) = entries.next_if(|e| e.data_offset <= covered) {
                if e.header_offset != expected {
                    return Err(Error::Inconsistent(format!(
                        "{} starts at {} but the previous member ended at {expected}",
                        e.path_lossy(),
                        e.header_offset
                    )));
                }
                self.check_headers(e)?;
                expected = e.data_offset + pad(e.size);
            }
        }
        if let Some(e) = entries.next() {
            return Err(Error::Inconsistent(format!(
                "{} lies past the end of the tar stream",
                e.path_lossy()
            )));
        }
        let last = *self.data.last().expect("open requires one");
        let terminator = self.frames[last];
        let zeros = self.decode_frame(last)?;
        if zeros.len() < 2 * BLOCK || zeros.iter().any(|&b| b != 0) {
            return format_err("the Terminator is not at least 1024 zero bytes (§5.4)");
        }
        if expected != terminator.stream_offset {
            return Err(Error::Inconsistent(format!(
                "the index accounts for {expected} bytes of tar stream before the Terminator, \
                 which starts at {}",
                terminator.stream_offset
            )));
        }
        Ok(())
    }

    /// Decode, in parallel, the frames from the one holding stream offset
    /// `from` onward — as many as fit in half the cache, and at least one —
    /// and return the stream offset where they end. Reading in stream order
    /// calls this whenever it reaches the end of the last window, so the
    /// frames it then reads are decoded already, on every core.
    pub fn prefetch(&self, from: u64) -> Result<u64> {
        let first = self.data.partition_point(|&i| {
            let f = &self.frames[i];
            f.stream_offset + f.decompressed_size <= from
        });
        let budget = (self.cache_bytes / 2).max(1);
        let mut window = Vec::new();
        let mut bytes = 0u64;
        for &i in &self.data[first..] {
            let size = self.frames[i].decompressed_size;
            if !window.is_empty() && bytes + size > budget {
                break;
            }
            window.push(i);
            bytes += size;
        }
        let Some(&last) = window.last() else {
            return Ok(self.index.tar_stream_size);
        };
        self.decode_frames(&window)?;
        let f = self.frames[last];
        Ok(f.stream_offset + f.decompressed_size)
    }
}

/// Lay the seek table's entries out as frames, reading each one's magic.
fn locate<S: Source + ?Sized>(source: &S, seek: &[SeekEntry]) -> Result<Vec<Frame>> {
    let mut frames = Vec::with_capacity(seek.len());
    let (mut file_offset, mut stream_offset) = (0u64, 0u64);
    for e in seek {
        let compressed = u64::from(e.compressed_size);
        if compressed < 8 {
            return format_err("a listed frame shorter than any frame header");
        }
        let magic = le32(&source.read_at(file_offset, 4)?);
        frames.push(Frame {
            file_offset,
            compressed_size: compressed,
            stream_offset,
            decompressed_size: u64::from(e.decompressed_size),
            checksum: e.checksum,
            magic,
        });
        file_offset += compressed;
        stream_offset += u64::from(e.decompressed_size);
    }
    Ok(frames)
}

/// Check the frame order of §3.1 and the §8.2 profile; return the data frames.
fn check_layout(frames: &[Frame]) -> Result<Vec<usize>> {
    if frames.len() < 3 {
        return format_err("fewer frames than a header, a Terminator and an index");
    }
    if frames[0].magic != MAGIC_HEADER {
        return format_err("the first frame is not the Archive Header (§3.1)");
    }
    if frames[frames.len() - 1].magic != MAGIC_INDEX {
        return format_err("the last listed frame is not the Central Index (§8.2)");
    }
    let mut data = Vec::new();
    for (i, f) in frames.iter().enumerate().take(frames.len() - 1).skip(1) {
        if f.is_data() {
            let d = f.decompressed_size;
            if d == 0 || d % BLOCK as u64 != 0 || d > frames::MAX_FRAME_CONTENT {
                return format_err(format!(
                    "data frame {i} holds {d} bytes: §5.2 wants whole tar blocks, at most 1 GiB"
                ));
            }
            data.push(i);
            continue;
        }
        if !zalt_codec::magic::is_zstd_skippable(f.magic) {
            return format_err(format!(
                "frame {i} is neither a Zstandard nor a skippable frame"
            ));
        }
        if f.decompressed_size != 0 {
            return format_err(format!("skippable frame {i} claims content (§8.2)"));
        }
        match f.magic {
            MAGIC_HEADER | MAGIC_INDEX | MAGIC_SEEK_TABLE => {
                return format_err(format!("frame {i} is a ZSTA structure out of place (§3.1)"));
            }
            MAGIC_DICTIONARY => {
                // §6 puts dictionaries before the first data frame; this
                // milestone does not read them at all.
                return Err(Error::Unsupported("dictionary frames (§6)".into()));
            }
            // Reserved and vendor frames: §3.2 forbids writers to place them
            // here, and Q7 is still open. A reader that skips them reads every
            // archive a writer that waits for Q7 could produce.
            _ => {}
        }
    }
    let Some(&terminator) = data.last() else {
        return format_err("no Terminator: there are no data frames (§5.4)");
    };
    if frames[terminator].decompressed_size < 2 * BLOCK as u64 {
        return format_err("the Terminator is shorter than 1024 bytes (§5.4)");
    }
    Ok(data)
}
