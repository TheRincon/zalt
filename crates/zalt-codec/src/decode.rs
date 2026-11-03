//! The streaming decoder.
//!
//! # Why this is a state machine from the first commit
//!
//! A browser feeds this socket-sized chunks and must produce output as they
//! arrive. That means suspending and resuming at *arbitrary* byte boundaries —
//! mid-magic, mid-frame-header, mid-block. Writing a one-shot decoder first and
//! streaming it later is a rewrite, not a refactor, because every
//! `read_exact`-shaped assumption has to be unwound.
//!
//! # Why compressed blocks are buffered whole
//!
//! Raw and RLE block payloads stream straight through. A compressed block
//! cannot: its literals section must be fully decoded before the sequences that
//! consume it can run, and its sequence bitstream is read *backwards* from the
//! end, so nothing can be decoded until the last byte has arrived.
//!
//! That is not a concession — it is bounded. `Block_Maximum_Size` is
//! `min(128 KiB, window_size)`, so the buffer has a hard ceiling the frame
//! header declares up front, and the window clamp (§6) bounds that in turn.
//! Streaming is preserved *between* blocks, which is where it matters.

use alloc::vec::Vec;

use crate::backend::{Backend, Scalar};
use crate::block::{BLOCK_HEADER_SIZE, BlockHeader, BlockType};
use crate::error::{Error, Result};
use crate::frame::{self, FrameHeader, FrameKind};
use crate::huf;
use crate::literals;
use crate::params::DecodeLimits;
use crate::sequences::{self, REP_COUNT, REP_INITIAL, Sequence, Tables};
use crate::window::Window;
use crate::xxh64::Xxh64;

/// What the decoder wants next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// All available input was consumed; call again with more.
    NeedMoreInput,
    /// A frame finished. More frames may follow — zstd streams may concatenate.
    FrameComplete,
}

/// Outcome of one [`Decoder::decompress`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes of `input` consumed.
    pub consumed: usize,
    /// Whether more input is required or a frame just completed.
    pub status: Status,
}

/// Accumulator for structures that may straddle an input chunk boundary.
#[derive(Debug, Clone)]
struct Scratch {
    buf: [u8; 16],
    len: usize,
}

impl Scratch {
    const fn new() -> Self {
        Self {
            buf: [0; 16],
            len: 0,
        }
    }

    fn fill(&mut self, input: &[u8], cursor: &mut usize, need: usize) -> bool {
        debug_assert!(need <= self.buf.len());
        while self.len < need && *cursor < input.len() {
            self.buf[self.len] = input[*cursor];
            self.len += 1;
            *cursor += 1;
        }
        self.len >= need
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

#[derive(Debug, Clone, Copy)]
enum State {
    Magic,
    SkippableLen,
    SkippableSkip { remaining: u32 },
    Descriptor,
    HeaderRest { descriptor: u8, need: usize },
    BlockHeader,
    RawData { remaining: u32, last: bool },
    RleByte { size: u32, last: bool },
    CompressedData { remaining: u32, last: bool },
    Checksum,
    FrameDone,
}

/// Where decoded bytes go as they are produced.
///
/// # Why this exists
///
/// Decoding writes every byte into the window, because a later match may reach
/// back to it. Handing the caller a `Vec` then copies each byte a second time,
/// and the checksum reads it a third — three touches where the reference
/// implementation manages two, and at 34 MB of output that copy was the single
/// largest item in the profile.
///
/// A caller that can consume a borrowed slice — writing to a socket, a file, a
/// hasher, a parser — never needs its own copy. It implements this instead and
/// the copy disappears. Bytes arrive as **runs, not whole blocks**, and a run
/// may be split in two where the window's ring wraps; a sink must therefore
/// treat successive calls as one continuous stream and must not assume
/// anything about where the boundaries fall.
///
/// `Vec<u8>` implements this, so the copying path is still one line and is
/// what [`Decoder::decompress`] does.
pub trait Sink {
    /// Accept the next run of decoded bytes.
    fn accept(&mut self, bytes: &[u8]);
}

impl Sink for Vec<u8> {
    fn accept(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// Adapt a closure into a [`Sink`].
///
/// A blanket `impl<F: FnMut(&[u8])> Sink for F` would be more convenient and
/// cannot be written: nothing proves `Vec<u8>` is not also `FnMut`, so it
/// collides with the impl above.
pub struct FnSink<F>(pub F);

impl<F: FnMut(&[u8])> Sink for FnSink<F> {
    fn accept(&mut self, bytes: &[u8]) {
        (self.0)(bytes);
    }
}

/// Length of the stack run used to expand an RLE block.
///
/// Big enough that the loop overhead is irrelevant, small enough to stay in
/// L1 and to cost nothing on the stack.
const RLE_RUN: usize = 256;

/// A streaming zstd decoder.
///
/// Generic over the acceleration [`Backend`], selected at compile time. The
/// default is [`Scalar`], which keeps this crate usable — and testable under
/// Miri — without any architecture-specific code.
#[derive(Debug, Clone)]
pub struct Decoder<B: Backend = Scalar> {
    limits: DecodeLimits,
    state: State,
    scratch: Scratch,
    frame: Option<FrameHeader>,
    hasher: Xxh64,
    decoded: u64,

    /// Whole compressed block, accumulated before decoding.
    block: Vec<u8>,
    /// Literals for the block being executed, reused across blocks.
    literals: Vec<u8>,
    /// Sequences for the block being executed, reused across blocks.
    sequence_buf: Vec<Sequence>,

    /// Huffman table carried across blocks, for treeless literals.
    huffman: huf::Table,
    /// FSE tables carried across blocks, for `Repeat` mode.
    tables: Tables,
    /// Repeat offsets, carried across blocks within a frame.
    repeats: [u32; REP_COUNT],
    /// Dictionary to decode with, if the host has loaded one.
    dictionary: Option<crate::dict::Dictionary>,
    /// Bounded match history. Owning this is what lets a caller drain and
    /// discard output instead of holding an entire frame.
    window: Window,

    backend: core::marker::PhantomData<B>,
}

impl<B: Backend> Default for Decoder<B> {
    fn default() -> Self {
        Self::new(DecodeLimits::default())
    }
}

impl<B: Backend> Decoder<B> {
    /// Create a decoder with the given bounds.
    pub fn new(limits: DecodeLimits) -> Self {
        Self {
            limits,
            state: State::Magic,
            scratch: Scratch::new(),
            frame: None,
            hasher: Xxh64::new(0),
            decoded: 0,
            block: Vec::new(),
            literals: Vec::new(),
            sequence_buf: Vec::new(),
            huffman: huf::Table::default(),
            tables: Tables::default(),
            repeats: REP_INITIAL,
            dictionary: None,
            window: Window::new(0),
            backend: core::marker::PhantomData,
        }
    }

    /// Whether the decoder sits between frames with nothing buffered — the one
    /// state in which the end of the input is the end of a complete stream.
    ///
    /// **Not the same as "the last call returned [`Status::FrameComplete`]".**
    /// A stream may end with a skippable frame, which finishes without one: the
    /// decoder steps over it and goes back to waiting for a magic number, and
    /// reports [`Status::NeedMoreInput`] because, mid-stream, more frames may
    /// follow. At the end of the input that is a clean finish, not a truncated
    /// one — `zstd(1)` agrees — and a caller deciding which needs this, not the
    /// last status. Every ZALT-archive ends in skippable frames, as does any
    /// file carrying zstd's seekable-format seek table.
    pub fn is_between_frames(&self) -> bool {
        matches!(self.state, State::Magic) && self.scratch.len == 0
    }

    /// Return to the initial state, ready for an unrelated stream.
    ///
    /// Keeps the allocated buffers, which is the point: reusing a decoder
    /// across responses avoids re-allocating a block buffer per request.
    pub fn reset(&mut self) {
        self.state = State::Magic;
        self.scratch.clear();
        self.frame = None;
        self.hasher = Xxh64::new(0);
        self.decoded = 0;
        self.block.clear();
        self.literals.clear();
        self.sequence_buf.clear();
        self.window.reset(0);
        self.reset_frame_state();
    }

    /// Clear everything that is scoped to a single frame.
    ///
    /// Entropy tables and repeat offsets persist *within* a frame and must not
    /// leak across one: a `Repeat` mode or treeless literals section in a new
    /// frame's first block has nothing to inherit, and silently reusing the
    /// previous frame's tables would decode garbage instead of erroring.
    fn reset_frame_state(&mut self) {
        self.huffman.invalidate();
        self.tables.invalidate();
        self.repeats = REP_INITIAL;
    }

    /// Decode subsequent frames with this dictionary.
    ///
    /// Takes effect at the next frame boundary, not mid-frame: a dictionary
    /// seeds history and entropy tables, both of which are frame-scoped, and
    /// swapping either underneath a frame in progress would decode the rest of
    /// it against state its first block never saw.
    ///
    /// The dictionary is not consulted unless a frame's `Dictionary_ID` matches
    /// it or the frame names none. Silently decoding a frame against the wrong
    /// dictionary is the one failure mode the ID exists to prevent.
    pub fn load_dictionary(&mut self, dictionary: crate::dict::Dictionary) {
        self.dictionary = Some(dictionary);
    }

    /// Forget any loaded dictionary, from the next frame onwards.
    pub fn clear_dictionary(&mut self) {
        self.dictionary = None;
    }

    /// The loaded dictionary's ID, if one is loaded.
    pub fn dictionary_id(&self) -> Option<u32> {
        self.dictionary.as_ref().map(|d| d.id())
    }

    /// Seed a frame from the loaded dictionary.
    ///
    /// Called once per frame, after `reset_frame_state` has cleared what the
    /// previous frame left. Order matters: the reset invalidates the entropy
    /// tables, so seeding has to follow it or the dictionary's tables are
    /// thrown away immediately.
    fn apply_dictionary(&mut self) {
        let Some(dict) = self.dictionary.as_ref() else {
            return;
        };

        self.window.set_dictionary(dict.content());

        let Some(entropy) = dict.entropy() else {
            // A raw-content dictionary is history and nothing else.
            return;
        };

        // Cloned rather than borrowed. The tables are frame state and a block
        // may overwrite any of them with a new description; the dictionary has
        // to survive intact for the next frame. This is the per-frame cost the
        // thirteenth pass removed from the *block* path, paid back once here —
        // and it buys skipping table construction for the frame's first block,
        // which §10 measures at ~18% of a small document's decode.
        self.huffman = entropy.literals.clone();
        self.tables.literals_length = entropy.literal_lengths.clone();
        self.tables.offset = entropy.offsets.clone();
        self.tables.match_length = entropy.match_lengths.clone();
        self.repeats = entropy.repeats;
    }

    /// Bytes of output produced for the frame in progress.
    pub fn decoded_len(&self) -> u64 {
        self.decoded
    }

    /// The frame header currently being decoded, once it has been parsed.
    pub fn frame_header(&self) -> Option<&FrameHeader> {
        self.frame.as_ref()
    }

    /// Bytes of heap this decoder currently holds.
    ///
    /// The embedding layer needs this to account for live decoders against a
    /// global budget; see `doc/ZALT-MAC.md` §6.
    pub fn heap_footprint(&self) -> usize {
        self.block.capacity()
            + self.literals.capacity()
            + self.sequence_buf.capacity() * core::mem::size_of::<Sequence>()
            + self.window.footprint()
    }

    /// Detach the working buffers, leaving the decoder empty.
    ///
    /// Resets decode state: a decoder without its buffers cannot continue a
    /// frame, so this is for returning memory to a host free list between
    /// responses, not for use mid-stream.
    pub fn take_buffers(&mut self) -> Buffers {
        self.reset();
        Buffers {
            block: core::mem::take(&mut self.block),
            literals: core::mem::take(&mut self.literals),
            sequences: core::mem::take(&mut self.sequence_buf),
            window: self.window.take_buffer(),
        }
    }

    /// Adopt caller-supplied buffers, keeping whichever of each is larger.
    ///
    /// Donating never shrinks the decoder: a host handing over a small buffer
    /// cannot accidentally discard a larger one already in use.
    pub fn donate_buffers(&mut self, buffers: Buffers) {
        fn adopt<T>(current: &mut Vec<T>, mut donated: Vec<T>) {
            donated.clear();
            if donated.capacity() > current.capacity() {
                *current = donated;
            }
        }
        adopt(&mut self.block, buffers.block);
        adopt(&mut self.literals, buffers.literals);
        adopt(&mut self.sequence_buf, buffers.sequences);
        self.window.donate_buffer(buffers.window);
    }

    /// Consume as much of `input` as possible, appending output to `out`.
    pub fn decompress(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Progress> {
        self.decompress_into(input, out)
    }

    /// Consume as much of `input` as possible, handing output to `sink`.
    ///
    /// The zero-copy form of [`Decoder::decompress`]: a sink that can consume
    /// borrowed runs never pays for a copy it did not need. See [`Sink`] for
    /// what a run is and is not.
    pub fn decompress_into<S: Sink>(&mut self, input: &[u8], out: &mut S) -> Result<Progress> {
        let mut cursor = 0usize;

        loop {
            match self.state {
                State::Magic => {
                    if !self.scratch.fill(input, &mut cursor, 4) {
                        return Ok(self.need_more(cursor));
                    }
                    let magic = u32::from_le_bytes([
                        self.scratch.buf[0],
                        self.scratch.buf[1],
                        self.scratch.buf[2],
                        self.scratch.buf[3],
                    ]);
                    self.scratch.clear();
                    self.state = match frame::classify(magic)? {
                        FrameKind::Zstd => State::Descriptor,
                        FrameKind::Skippable => State::SkippableLen,
                    };
                }

                State::SkippableLen => {
                    if !self.scratch.fill(input, &mut cursor, 4) {
                        return Ok(self.need_more(cursor));
                    }
                    let len = u32::from_le_bytes([
                        self.scratch.buf[0],
                        self.scratch.buf[1],
                        self.scratch.buf[2],
                        self.scratch.buf[3],
                    ]);
                    self.scratch.clear();
                    self.state = State::SkippableSkip { remaining: len };
                }

                State::SkippableSkip { remaining } => {
                    let available = input.len() - cursor;
                    let take = (remaining as usize).min(available);
                    cursor += take;
                    let left = remaining - take as u32;
                    if left > 0 {
                        self.state = State::SkippableSkip { remaining: left };
                        return Ok(self.need_more(cursor));
                    }
                    self.state = State::Magic;
                }

                State::Descriptor => {
                    if !self.scratch.fill(input, &mut cursor, 1) {
                        return Ok(self.need_more(cursor));
                    }
                    let descriptor = self.scratch.buf[0];
                    self.scratch.clear();
                    let need = frame::header_len_after_descriptor(descriptor)?;
                    self.state = State::HeaderRest { descriptor, need };
                }

                State::HeaderRest { descriptor, need } => {
                    if !self.scratch.fill(input, &mut cursor, need) {
                        return Ok(self.need_more(cursor));
                    }
                    let header =
                        FrameHeader::parse(descriptor, &self.scratch.buf[..need], &self.limits)?;
                    self.scratch.clear();

                    // A frame naming a dictionary can only be decoded with
                    // that dictionary. Decoding it against a different one, or
                    // none, produces plausible output from the wrong history —
                    // which is precisely what the ID is in the format to stop.
                    if let Some(id) = header.dictionary_id
                        && self.dictionary.as_ref().map(|d| d.id()) != Some(id)
                    {
                        return Err(Error::DictionaryRequired { id });
                    }

                    // Arm the window at the declared ceiling. Nothing is
                    // allocated here — capacity is a bound that growth
                    // approaches, so a frame claiming 8 MiB and producing 4 KB
                    // costs 4 KB.
                    let capacity =
                        usize::try_from(header.window_size).map_err(|_| Error::TooLarge)?;
                    self.window.reset(capacity);

                    self.frame = Some(header);
                    self.hasher = Xxh64::new(0);
                    self.decoded = 0;
                    self.reset_frame_state();
                    self.apply_dictionary();
                    self.state = State::BlockHeader;
                }

                State::BlockHeader => {
                    if !self.scratch.fill(input, &mut cursor, BLOCK_HEADER_SIZE) {
                        return Ok(self.need_more(cursor));
                    }
                    let bytes = [
                        self.scratch.buf[0],
                        self.scratch.buf[1],
                        self.scratch.buf[2],
                    ];
                    self.scratch.clear();
                    let header = BlockHeader::parse(bytes)?;

                    let frame = self.frame.ok_or(Error::Malformed)?;
                    if u64::from(header.size) > frame.block_size_max() {
                        return Err(Error::TooLarge);
                    }

                    self.state = match header.block_type {
                        BlockType::Raw => State::RawData {
                            remaining: header.size,
                            last: header.last,
                        },
                        BlockType::Rle => State::RleByte {
                            size: header.size,
                            last: header.last,
                        },
                        BlockType::Compressed => {
                            self.block.clear();
                            self.block.reserve(header.size as usize);
                            State::CompressedData {
                                remaining: header.size,
                                last: header.last,
                            }
                        }
                    };
                }

                State::RawData { remaining, last } => {
                    if remaining > 0 {
                        let available = input.len() - cursor;
                        if available == 0 {
                            return Ok(self.need_more(cursor));
                        }
                        let take = (remaining as usize).min(available);
                        self.emit(out, &input[cursor..cursor + take])?;
                        cursor += take;
                        let left = remaining - take as u32;
                        if left > 0 {
                            self.state = State::RawData {
                                remaining: left,
                                last,
                            };
                            return Ok(self.need_more(cursor));
                        }
                    }
                    self.state = self.after_block(last);
                }

                State::RleByte { size, last } => {
                    if !self.scratch.fill(input, &mut cursor, 1) {
                        return Ok(self.need_more(cursor));
                    }
                    let byte = self.scratch.buf[0];
                    self.scratch.clear();

                    // An RLE block is one byte and a repeat count that reaches
                    // `Block_Maximum_Size`. Materialising it in the sink's
                    // buffer would need a `Vec`; a fixed run emitted repeatedly
                    // needs nothing, keeps the sink generic, and stays in L1
                    // cache instead of touching 128 KiB of fresh pages.
                    let run = [byte; RLE_RUN];
                    let mut left = size as usize;
                    while left > 0 {
                        let take = left.min(RLE_RUN);
                        self.emit(out, &run[..take])?;
                        left -= take;
                    }

                    self.state = self.after_block(last);
                }

                State::CompressedData { remaining, last } => {
                    if remaining > 0 {
                        let available = input.len() - cursor;
                        if available == 0 {
                            return Ok(self.need_more(cursor));
                        }
                        let take = (remaining as usize).min(available);
                        self.block.extend_from_slice(&input[cursor..cursor + take]);
                        cursor += take;
                        let left = remaining - take as u32;
                        if left > 0 {
                            self.state = State::CompressedData {
                                remaining: left,
                                last,
                            };
                            return Ok(self.need_more(cursor));
                        }
                    }
                    self.decode_compressed_block(out)?;
                    self.state = self.after_block(last);
                }

                State::Checksum => {
                    if !self.scratch.fill(input, &mut cursor, 4) {
                        return Ok(self.need_more(cursor));
                    }
                    let stored = u32::from_le_bytes([
                        self.scratch.buf[0],
                        self.scratch.buf[1],
                        self.scratch.buf[2],
                        self.scratch.buf[3],
                    ]);
                    self.scratch.clear();
                    if stored != self.hasher.frame_checksum() {
                        return Err(Error::ChecksumMismatch);
                    }
                    self.state = State::FrameDone;
                }

                State::FrameDone => {
                    self.verify_declared_size()?;
                    self.state = State::Magic;
                    self.frame = None;
                    return Ok(Progress {
                        consumed: cursor,
                        status: Status::FrameComplete,
                    });
                }
            }
        }
    }

    /// Decode one fully-buffered compressed block into `out`.
    fn decode_compressed_block<S: Sink>(&mut self, out: &mut S) -> Result<()> {
        let frame = self.frame.ok_or(Error::Malformed)?;
        let before = self.window.total();

        self.literals.clear();
        let consumed = {
            let block = core::mem::take(&mut self.block);
            let result = literals::decode(
                &block,
                &mut self.huffman,
                &mut self.literals,
                frame.block_size_max(),
            );
            self.block = block;
            result?
        };

        self.sequence_buf.clear();
        {
            let block = core::mem::take(&mut self.block);
            let rest = block.get(consumed..).ok_or(Error::Truncated);
            let result = rest.and_then(|rest| {
                sequences::decode(
                    rest,
                    &mut self.tables,
                    &mut self.repeats,
                    &mut self.sequence_buf,
                    frame.block_size_max(),
                )
            });
            self.block = block;
            result?;
        }

        // Both buffers are taken out of `self` for the duration: execution needs
        // `&mut self` for the window while reading from them.
        let sequences = core::mem::take(&mut self.sequence_buf);
        let literals = core::mem::take(&mut self.literals);
        let result = self.execute(&sequences, &literals, frame);
        self.sequence_buf = sequences;
        self.literals = literals;
        result?;

        let produced =
            usize::try_from(self.window.total() - before).map_err(|_| Error::TooLarge)?;
        self.drain(out, produced)
    }

    /// Emit literals and copy matches for one block's sequences.
    fn execute(
        &mut self,
        sequences: &[Sequence],
        literals: &[u8],
        frame: FrameHeader,
    ) -> Result<()> {
        let cap = self.block_output_cap(frame)?;
        // Output produced by this block so far. Every increment is checked
        // before the bytes it accounts for are written.
        let mut produced: u64 = 0;
        let mut literal_pos = 0usize;

        for seq in sequences.iter() {
            // Ask for the *next* sequence's match source before executing this
            // one. A match reads from wherever the encoder pointed it, and by
            // the time the copy runs there is nothing left to overlap the load
            // with; one sequence of notice is roughly a literal run plus a
            // match copy of latency to hide behind.
            let ll = seq.literal_length as usize;
            let end = literal_pos.checked_add(ll).ok_or(Error::Malformed)?;
            let chunk = literals.get(literal_pos..end).ok_or(Error::Malformed)?;
            produced += ll as u64;
            if produced > cap {
                return Err(Error::TooLarge);
            }
            self.window.write_all::<B>(chunk);
            literal_pos = end;

            // A match may reach back only as far as the window allows. The
            // window enforces its own retained history too, but checking the
            // frame's declared size here distinguishes "beyond what this frame
            // may address" from "beyond what we happen to still hold".
            // Plus whatever of the dictionary is still reachable. RFC 8878 §5
            // permits a sequence to name "parts of the dictionary with offsets
            // larger than `Window_Size`" until the frame's output passes that
            // size, so a bound of `window_size` alone refuses legal frames —
            // which it did, on the first reference-compressed frame tried.
            let addressable = frame.window_size + self.window.dict_reach() as u64;
            if u64::from(seq.offset) > addressable {
                return Err(Error::OffsetTooLarge {
                    offset: u64::from(seq.offset),
                    available: addressable,
                });
            }
            produced += u64::from(seq.match_length);
            if produced > cap {
                return Err(Error::TooLarge);
            }
            self.window
                .copy_match::<B>(seq.offset as usize, seq.match_length as usize)?;
        }

        // Literals the sequences did not consume are appended verbatim; a block
        // may be nothing but literals.
        let tail = literals.get(literal_pos..).ok_or(Error::Malformed)?;
        if !tail.is_empty() {
            produced += tail.len() as u64;
            if produced > cap {
                return Err(Error::TooLarge);
            }
            self.window.write_all::<B>(tail);
        }
        Ok(())
    }

    /// The ceiling on this block's output, resolved once.
    ///
    /// `block_size_max`, `window.total()` and two `Option`s were re-examined
    /// twice per sequence for a value that cannot move during a block:
    /// `self.decoded` only advances when the block drains, which happens after
    /// execution returns. One `u64` compare against a running counter is the
    /// same check, and the limit is still enforced *before* each run is
    /// produced, which is the property that matters.
    fn block_output_cap(&self, frame: FrameHeader) -> Result<u64> {
        let ceiling = match (self.limits.max_decoded_size, frame.frame_content_size) {
            (Some(limit), Some(declared)) => Some(limit.min(declared)),
            (limit, declared) => limit.or(declared),
        };
        let room = match ceiling {
            Some(c) => c.checked_sub(self.decoded).ok_or(Error::TooLarge)?,
            None => u64::MAX,
        };
        Ok(frame.block_size_max().min(room))
    }

    /// Copy freshly produced bytes out of the window to the caller.
    ///
    /// Two slices, because the range may straddle the ring's wrap point.
    fn drain<S: Sink>(&mut self, out: &mut S, produced: usize) -> Result<()> {
        {
            // Split the borrow by field: the checksum reads the window
            // directly rather than reading back what was just written to the
            // caller. Those bytes are the ones the decoder touched last and are
            // still hot; the caller's buffer may be anywhere, and with a
            // zero-copy sink there is no caller's buffer to read at all.
            let Self {
                window,
                hasher,
                frame,
                ..
            } = &mut *self;
            let (a, b) = window.recent(produced)?;
            // Only frames that declare a checksum need one computed. Hashing
            // regardless costs a full pass over every byte of output for a
            // value nothing will ever compare — around 7% of decode time on a
            // frame written with `--no-check`.
            if frame.is_some_and(|f| f.has_checksum) {
                hasher.update(a);
                hasher.update(b);
            }
            out.accept(a);
            out.accept(b);
        }
        self.decoded += produced as u64;
        Ok(())
    }

    fn need_more(&self, consumed: usize) -> Progress {
        Progress {
            consumed,
            status: Status::NeedMoreInput,
        }
    }

    fn after_block(&self, last: bool) -> State {
        if !last {
            return State::BlockHeader;
        }
        match self.frame {
            Some(f) if f.has_checksum => State::Checksum,
            _ => State::FrameDone,
        }
    }

    fn check_limits(&self, additional: u64) -> Result<()> {
        let total = self
            .decoded
            .checked_add(additional)
            .ok_or(Error::TooLarge)?;
        if let Some(limit) = self.limits.max_decoded_size
            && total > limit
        {
            return Err(Error::TooLarge);
        }
        if let Some(declared) = self.frame.and_then(|f| f.frame_content_size)
            && total > declared
        {
            return Err(Error::TooLarge);
        }
        Ok(())
    }

    /// Emit raw-block bytes: straight through to the caller, and into the
    /// window so later matches can still reach them.
    fn emit<S: Sink>(&mut self, out: &mut S, bytes: &[u8]) -> Result<()> {
        self.check_limits(bytes.len() as u64)?;
        self.window.write_all::<B>(bytes);
        out.accept(bytes);
        if self.frame.is_some_and(|f| f.has_checksum) {
            self.hasher.update(bytes);
        }
        self.decoded += bytes.len() as u64;
        Ok(())
    }

    fn verify_declared_size(&self) -> Result<()> {
        if let Some(declared) = self.frame.and_then(|f| f.frame_content_size)
            && self.decoded != declared
        {
            return Err(Error::Malformed);
        }
        Ok(())
    }
}

/// A decoder's working buffers, detached from it.
///
/// # Why this instead of a custom allocator
///
/// A host embedding this library — a browser especially — wants to know and
/// control what it allocates. The obvious mechanism, a per-allocation custom
/// allocator, needs the unstable `allocator_api`, and a codec that only builds
/// on nightly is not one anybody can ship.
///
/// Buffer donation is the stable answer, and it is the one the reference
/// implementation reaches for too (`ZSTD_initStaticDCtx` and its workspace).
/// The host allocates, keeps a free list, and lends buffers to decoders; the
/// library never grows memory the host did not hand it, beyond what it reports.
///
/// Capacity is what is being donated. Contents are always cleared.
#[derive(Debug, Default)]
pub struct Buffers {
    /// Backing store for whole compressed blocks.
    pub block: Vec<u8>,
    /// Backing store for a block's decoded literals.
    pub literals: Vec<u8>,
    /// Backing store for a block's sequences.
    pub sequences: Vec<Sequence>,
    /// Backing store for the match window.
    pub window: Vec<u8>,
}

impl Buffers {
    /// Total capacity held, in bytes.
    pub fn capacity(&self) -> usize {
        self.block.capacity()
            + self.literals.capacity()
            + self.sequences.capacity() * core::mem::size_of::<Sequence>()
            + self.window.capacity()
    }
}

/// A decoder using the portable scalar backend.
///
/// Rust's default type parameters do not participate in inference, so
/// `Decoder::new(..)` cannot pick `Scalar` on its own. Name this alias, or
/// write `Decoder::<Scalar>::new(..)`.
pub type ScalarDecoder = Decoder<Scalar>;

/// Decode a complete frame from a contiguous buffer, using the scalar backend.
pub fn decompress(input: &[u8], limits: DecodeLimits) -> Result<Vec<u8>> {
    decompress_with::<Scalar>(input, limits)
}

/// How much output buffer to allocate up front, from the frame's own claim.
///
/// # Why this is bounded rather than trusted
///
/// `Frame_Content_Size` is attacker-controlled and reaches `u64::MAX`. Passing
/// it to `Vec::with_capacity` would be the window amplification of §6 wearing a
/// different hat: a nine-byte header that reserves terabytes, before a single
/// block has been shown to decode.
///
/// So the claim is only ever an *upper* hint. It is clamped to the caller's own
/// declared limit where there is one, and to a modest ceiling where there is
/// not — growth past that is the `Vec`'s ordinary amortised doubling, which
/// costs a few reallocations on a genuinely large frame and nothing on a small
/// one. A frame that lies about being large gets one bounded allocation and
/// then fails on its content like any other malformed input.
fn initial_output_capacity(input: &[u8], limits: &DecodeLimits) -> usize {
    /// Enough that ordinary responses never reallocate, small enough that a
    /// lying header cannot turn one request into a memory event.
    const UNVERIFIED_MAX: u64 = 1 << 20;

    let Some(declared) = peek_content_size(input, limits) else {
        return 0;
    };
    let ceiling = limits.max_decoded_size.unwrap_or(UNVERIFIED_MAX);
    usize::try_from(declared.min(ceiling)).unwrap_or(0)
}

/// Read `Frame_Content_Size` from a frame header without decoding anything.
fn peek_content_size(input: &[u8], limits: &DecodeLimits) -> Option<u64> {
    let descriptor = *input.get(4)?;
    if u32::from_le_bytes(input.get(..4)?.try_into().ok()?) != crate::magic::ZSTD_FRAME {
        return None;
    }
    let need = frame::header_len_after_descriptor(descriptor).ok()?;
    let rest = input.get(5..5 + need)?;
    FrameHeader::parse(descriptor, rest, limits)
        .ok()?
        .frame_content_size
}

/// Decode a complete frame from a contiguous buffer with a chosen backend.
pub fn decompress_with<B: Backend>(input: &[u8], limits: DecodeLimits) -> Result<Vec<u8>> {
    let mut decoder = Decoder::<B>::new(limits);
    let mut out = Vec::with_capacity(initial_output_capacity(input, &limits));
    let mut cursor = 0usize;

    loop {
        let progress = decoder.decompress(&input[cursor..], &mut out)?;
        cursor += progress.consumed;
        match progress.status {
            Status::FrameComplete if cursor >= input.len() => return Ok(out),
            Status::FrameComplete => continue,
            // All input consumed, back between frames: a stream that ended on
            // a skippable frame, complete. Empty input is still refused — a
            // stream with no frames at all is not a stream.
            Status::NeedMoreInput
                if progress.consumed == 0
                    && cursor == input.len()
                    && cursor > 0
                    && decoder.is_between_frames() =>
            {
                return Ok(out);
            }
            Status::NeedMoreInput if progress.consumed == 0 => return Err(Error::Truncated),
            Status::NeedMoreInput => continue,
        }
    }
}
