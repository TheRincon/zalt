//! Appending to a finished archive (§10).
//!
//! The Terminator sits in a frame of its own (§5.4) precisely so this works
//! without re-encoding anything: the file is cut where the Terminator starts,
//! and a writer resumes there — new data frames, a new Terminator, then a new
//! Central Index holding every old record unchanged (their offsets have not
//! moved) and the new ones, and a new seek table.
//!
//! ## Not atomic, by the spec's design
//!
//! From [`append`](fn@append) until [`Writer::finish`] returns, the file has no trailer.
//! A crash, an error — a refused duplicate path, a file that cannot be read —
//! or a writer dropped unfinished leaves it that way. Nothing is lost:
//! [`crate::recover`](fn@crate::recover) rebuilds a trailer from what is there, and since the old
//! members are untouched it gets all of them back, plus any new ones whose
//! frames were complete. A caller that needs the file never to be in that
//! state appends to a copy and renames it into place, as §10 suggests and
//! `zalt archive append` does.

use std::io::{BufWriter, Seek, SeekFrom};

use crate::layout::SeekEntry;
use crate::read::{Archive, ReadLimits};
use crate::tar::BLOCK;
use crate::write::{Resume, Writer, WriterOptions};
use crate::{Error, Result, format_err, frames};

/// Open `file` — readable, writable, a complete archive — for appending.
///
/// The archive is opened by its index (§11.1) and its Terminator checked
/// (§5.4) before anything is changed. Then the file is cut at the Terminator
/// and the returned writer adds after it; [`Writer::finish`] writes the new
/// trailer. `options` sets how the *new* frames are written; the Archive
/// Header, and with it the `Creator`, stays as it was.
///
/// **Dictionaries (§6).** A Dictionary frame cannot be added by appending —
/// §6 puts them before the first data frame — so `options.dictionary` must be
/// one the archive holds, or it is refused. Left `None`, the new frames are
/// written against the dictionary the old Terminator named, if it named one:
/// an archive goes on being compressed the way it was, and a portable archive
/// stays portable.
pub fn append(
    file: std::fs::File,
    options: WriterOptions,
) -> Result<Writer<BufWriter<std::fs::File>>> {
    let io = |what: &str, e: std::io::Error| Error::Io(format!("appending: {what}"), e);
    let reader = file
        .try_clone()
        .map_err(|e| io("duplicating the handle", e))?;
    let archive = Archive::open(Box::new(reader), ReadLimits::default())?;

    // §10 step 1: the Terminator is the data frame just before the Central
    // Index, which is the last listed frame.
    let frames = archive.frames();
    let t = frames
        .len()
        .checked_sub(2)
        .ok_or_else(|| Error::Format("no frame before the Central Index".into()))?;
    let terminator = frames[t];
    if !terminator.is_data() {
        return format_err("the frame before the Central Index is not a data frame (§10)");
    }
    let content = archive.decode_frame(t)?;
    let options = dictionary_for_append(&archive, &file, &terminator, options)?;
    let ok =
        content.len() >= 2 * BLOCK && content.len() % BLOCK == 0 && content.iter().all(|&b| b == 0);
    if !ok {
        return format_err("the Terminator is not whole blocks of zeros (§5.4)");
    }

    let mut seek = Vec::with_capacity(t);
    for f in &frames[..t] {
        let checksum = match (f.is_data(), f.checksum) {
            (false, _) => Some(0),
            (true, Some(c)) => Some(c),
            (true, None) => {
                return Err(Error::Unsupported(
                    "appending to an archive whose seek table has no checksums".into(),
                ));
            }
        };
        seek.push(SeekEntry {
            compressed_size: f.compressed_size as u32,
            decompressed_size: f.decompressed_size as u32,
            checksum,
        });
    }
    let resume = Resume {
        seek,
        entries: archive.entries().to_vec(),
        stream: terminator.stream_offset,
        written: terminator.file_offset,
        data_frames: frames[..t].iter().filter(|f| f.is_data()).count() as u64,
        hash_algorithm: archive.hash_algorithm(),
    };
    drop(archive);

    // §10 step 2. From here until `finish`, the file has no trailer.
    let mut file = file;
    file.set_len(terminator.file_offset)
        .map_err(|e| io("cutting at the Terminator", e))?;
    file.seek(SeekFrom::Start(terminator.file_offset))
        .map_err(|e| io("seeking to the Terminator", e))?;
    Writer::resume(BufWriter::new(file), options, resume)
}

/// The options appended frames are written with: `options`, its dictionary
/// settled as [`append`](fn@append) describes.
fn dictionary_for_append(
    archive: &Archive<std::fs::File>,
    file: &std::fs::File,
    terminator: &crate::read::Frame,
    options: WriterOptions,
) -> Result<WriterOptions> {
    let held = archive.dictionaries();
    if let Some(d) = &options.dictionary {
        if !held.contains(d) {
            return Err(Error::Refused(format!(
                "the archive does not hold dictionary {:#010x}, and appending cannot add one (§6, §10)",
                d.id()
            )));
        }
        return Ok(options);
    }
    use std::os::unix::fs::FileExt;
    let mut head =
        vec![0u8; (zalt_codec::encode::MAX_HEADER).min(terminator.compressed_size as usize)];
    file.read_exact_at(&mut head, terminator.file_offset)
        .map_err(|e| Error::Io("reading the Terminator".into(), e))?;
    let named =
        frames::header_of(&head, &zalt_codec::params::DecodeLimits::default())?.dictionary_id;
    let dictionary = match named {
        None => None,
        // `open` decoded the Terminator, so the archive holds what it names.
        Some(id) => crate::dict::find(held, id).cloned(),
    };
    Ok(WriterOptions {
        dictionary,
        ..options
    })
}
