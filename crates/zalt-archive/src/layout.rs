//! The fixed structures: magics, skippable frames, the Archive Header (§4) and
//! the seek table (§8).
//!
//! Everything here is written from the spec, and every parser is strict in the
//! direction the spec asks. A reader that accepts more than the spec allows is
//! a reader two writers can disagree with, and §14 defines conformance by what
//! *other* readers accept.

use crate::{Error, Result, format_err};

// ------------------------------------------------------------------- magics

/// Archive Header skippable frame (§3.2).
pub const MAGIC_HEADER: u32 = 0x184D_2A50;
/// Dictionary skippable frame (§3.2, §6).
pub const MAGIC_DICTIONARY: u32 = 0x184D_2A51;
/// Central Index skippable frame (§3.2, §7).
pub const MAGIC_INDEX: u32 = 0x184D_2A52;
/// Seek table skippable frame, as libzstd's seekable format defines it.
///
/// The same value as Compression Dictionary Transport's prelude
/// (`zalt_codec::magic::ZSTD_SKIPPABLE_DCZ`). The two coexist by position
/// alone: `dcz` is at the head of a stream with a payload of exactly 32 bytes,
/// the seek table is at the tail and ends with [`SEEKABLE_MAGIC`]. A ZLTA
/// archive begins with [`MAGIC_HEADER`], so a `dcz` reader never mistakes it.
pub const MAGIC_SEEK_TABLE: u32 = 0x184D_2A5E;
/// Vendor-private skippable frame; `User_Data` begins with a vendor tag.
pub const MAGIC_VENDOR: u32 = 0x184D_2A5F;
/// The seek table footer's own magic, which is what actually identifies it.
pub const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;

/// Start of the Archive Header's `User_Data`.
pub const ARCHIVE_MAGIC: [u8; 4] = *b"ZLTA";
/// The format this crate reads and writes: 0.2. While the major version is 0,
/// §4.1 requires an exact match in both directions.
pub const VERSION: (u8, u8) = (0, 2);

/// `Archive_Flags` bit 0 (§4.2).
pub const FLAG_DICT_REQUIRED: u16 = 1;

/// Is this the magic of a standard Zstandard frame?
pub fn is_zstd_frame(magic: u32) -> bool {
    magic == zalt_codec::magic::ZSTD_FRAME
}

pub(crate) fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

pub(crate) fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// Wrap `user_data` in a skippable frame.
pub fn skippable(magic: u32, user_data: &[u8]) -> Result<Vec<u8>> {
    debug_assert!(zalt_codec::magic::is_zstd_skippable(magic));
    let size = u32::try_from(user_data.len())
        .map_err(|_| Error::Refused("a skippable frame's payload must fit in 4 GiB".into()))?;
    let mut out = Vec::with_capacity(8 + user_data.len());
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(user_data);
    Ok(out)
}

/// The `User_Data` of a skippable frame, checking its magic and that its
/// declared size is exactly the bytes given.
pub fn skippable_payload(frame: &[u8], magic: u32) -> Result<&[u8]> {
    if frame.len() < 8 {
        return format_err("skippable frame shorter than its header");
    }
    if le32(frame) != magic {
        return format_err(format!(
            "expected magic {magic:#010X}, found {:#010X}",
            le32(frame)
        ));
    }
    if le32(&frame[4..]) as usize != frame.len() - 8 {
        return format_err("skippable frame size disagrees with the seek table");
    }
    Ok(&frame[8..])
}

// ------------------------------------------------------------------ header

/// Header TLV tags (§4.3).
pub const TAG_CREATOR: u16 = 0x0001;
pub const TAG_CREATED: u16 = 0x0002;
pub const TAG_WRITER_POLICY: u16 = 0x0003;

/// The Archive Header (§4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    /// `Archive_Flags`.
    pub flags: u16,
    /// Every TLV, in file order, known or not.
    pub tlvs: Vec<(u16, Vec<u8>)>,
}

impl Header {
    /// The `Creator` TLV, if present.
    pub fn creator(&self) -> Option<&[u8]> {
        self.tlv(TAG_CREATOR)
    }

    /// The `Created` TLV, as Unix nanoseconds, if present and well-formed.
    pub fn created_ns(&self) -> Option<u64> {
        self.tlv(TAG_CREATED).filter(|v| v.len() == 8).map(le64)
    }

    fn tlv(&self, tag: u16) -> Option<&[u8]> {
        self.tlvs
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_slice())
    }

    /// The header's `User_Data`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(&ARCHIVE_MAGIC);
        out.push(VERSION.0);
        out.push(VERSION.1);
        out.extend_from_slice(&self.flags.to_le_bytes());
        for (tag, value) in &self.tlvs {
            let len = u16::try_from(value.len())
                .map_err(|_| Error::Refused(format!("header TLV {tag:#06x} longer than 64 KiB")))?;
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(value);
        }
        Ok(out)
    }

    /// Parse and validate a header's `User_Data` (§4.1, §4.2).
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return format_err("Archive Header shorter than 8 bytes");
        }
        if data[..4] != ARCHIVE_MAGIC {
            return format_err("Archive Header does not begin with \"ZLTA\"");
        }
        if (data[4], data[5]) != VERSION {
            // §4.1: while the major version is 0, every draft may change the
            // format, so a reader must match exactly — in both directions.
            return Err(Error::Unsupported(format!(
                "format version {}.{}; this reader implements exactly {}.{}",
                data[4], data[5], VERSION.0, VERSION.1
            )));
        }
        let flags = u16::from_le_bytes([data[6], data[7]]);
        if flags & !FLAG_DICT_REQUIRED != 0 {
            return format_err(format!("reserved Archive_Flags set: {flags:#06x}"));
        }
        let mut tlvs = Vec::new();
        let mut rest = &data[8..];
        while !rest.is_empty() {
            if rest.len() < 4 {
                return format_err("truncated header TLV");
            }
            let tag = u16::from_le_bytes([rest[0], rest[1]]);
            let len = usize::from(u16::from_le_bytes([rest[2], rest[3]]));
            if rest.len() < 4 + len {
                return format_err(format!("header TLV {tag:#06x} runs past the header"));
            }
            tlvs.push((tag, rest[4..4 + len].to_vec()));
            rest = &rest[4 + len..];
        }
        Ok(Self { flags, tlvs })
    }
}

// -------------------------------------------------------------- seek table

/// One seek table entry: a frame's size on disk and decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeekEntry {
    pub compressed_size: u32,
    pub decompressed_size: u32,
    /// Low 32 bits of XXH64 of the decoded content; 0 for skippable frames.
    pub checksum: Option<u32>,
}

/// Bytes of the seek table footer.
pub const FOOTER_LEN: usize = 9;

/// Encode the seek table frame, with checksums (§8.2: writers SHOULD).
pub fn encode_seek_table(entries: &[SeekEntry]) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(entries.len() * 12 + FOOTER_LEN);
    for e in entries {
        data.extend_from_slice(&e.compressed_size.to_le_bytes());
        data.extend_from_slice(&e.decompressed_size.to_le_bytes());
        data.extend_from_slice(&e.checksum.unwrap_or(0).to_le_bytes());
    }
    let count =
        u32::try_from(entries.len()).map_err(|_| Error::Refused("more than 2^32 frames".into()))?;
    data.extend_from_slice(&count.to_le_bytes());
    data.push(0x80); // Checksum_Flag; reserved bits zero
    data.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
    skippable(MAGIC_SEEK_TABLE, &data)
}

/// The seek table, read from the end of a file of `file_len` bytes, through
/// `read_at` — §11.1 steps 1 to 4.
///
/// Returns the entries and the seek table frame's own length.
///
/// Step 4 is the one the reference reader skips: the listed frames, plus the
/// seek table itself, must cover the file *exactly*. Without it a table can
/// describe a file shorter or longer than the one on disk, and every offset
/// derived from it is wrong by the difference.
pub fn read_seek_table(
    file_len: u64,
    mut read_at: impl FnMut(u64, usize) -> Result<Vec<u8>>,
    max_entries: u64,
) -> Result<(Vec<SeekEntry>, u64)> {
    if file_len < (8 + FOOTER_LEN) as u64 {
        return format_err("file too short to hold a seek table");
    }
    let footer = read_at(file_len - FOOTER_LEN as u64, FOOTER_LEN)?;
    if le32(&footer[5..]) != SEEKABLE_MAGIC {
        return format_err("no seek table: the file does not end with 0x8F92EAB1");
    }
    let descriptor = footer[4];
    if (descriptor >> 2) & 0x1F != 0 {
        return format_err("reserved bits set in Seek_Table_Descriptor");
    }
    let checksums = descriptor & 0x80 != 0;
    let count = u64::from(le32(&footer));
    if count > max_entries {
        return Err(Error::Refused(format!(
            "seek table lists {count} frames, over the limit of {max_entries}"
        )));
    }
    let entry_len: u64 = if checksums { 12 } else { 8 };
    let frame_len = 8 + count * entry_len + FOOTER_LEN as u64;
    if frame_len > file_len {
        return format_err("seek table longer than the file");
    }
    let frame = read_at(file_len - frame_len, frame_len as usize)?;
    let body = skippable_payload(&frame, MAGIC_SEEK_TABLE)?;

    let mut entries = Vec::with_capacity(count as usize);
    let mut covered = frame_len;
    for chunk in body[..body.len() - FOOTER_LEN].chunks_exact(entry_len as usize) {
        let e = SeekEntry {
            compressed_size: le32(chunk),
            decompressed_size: le32(&chunk[4..]),
            checksum: checksums.then(|| le32(&chunk[8..])),
        };
        covered = covered
            .checked_add(u64::from(e.compressed_size))
            .ok_or_else(|| Error::Format("frame sizes overflow".into()))?;
        entries.push(e);
    }
    if covered != file_len {
        return format_err(format!(
            "seek table covers {covered} bytes of a {file_len}-byte file"
        ));
    }
    Ok((entries, frame_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zalt_codec::magic;

    /// Every ZLTA magic is a zstd skippable magic, and none is a frame,
    /// dictionary or zalt container magic — the collision check the codec's
    /// own registry makes, extended to the values this crate adds.
    ///
    /// One overlap is deliberate and asserted as such: the seek table and
    /// `dcz` share `0x184D2A5E`. See [`MAGIC_SEEK_TABLE`].
    #[test]
    fn archive_magics_are_skippable_and_collide_only_where_the_ecosystem_does() {
        let ours = [
            MAGIC_HEADER,
            MAGIC_DICTIONARY,
            MAGIC_INDEX,
            MAGIC_SEEK_TABLE,
            MAGIC_VENDOR,
        ];
        for (i, m) in ours.iter().enumerate() {
            assert!(magic::is_zstd_skippable(*m), "{m:#X}");
            for other in &ours[i + 1..] {
                assert_ne!(m, other);
            }
            for codec in [
                magic::ZSTD_FRAME,
                magic::ZSTD_DICTIONARY,
                magic::ZALT_NATIVE,
            ] {
                assert_ne!(*m, codec);
            }
        }
        assert_eq!(
            MAGIC_SEEK_TABLE,
            magic::ZSTD_SKIPPABLE_DCZ,
            "shared by design with dcz; disambiguated by position"
        );
        assert!(!magic::is_zstd_skippable(SEEKABLE_MAGIC));
    }

    #[test]
    fn a_header_round_trips_and_its_flags_and_version_are_strict() {
        let h = Header {
            flags: 0,
            tlvs: vec![
                (TAG_CREATOR, b"zalt 0.0.1".to_vec()),
                (0x8001, vec![1, 2, 3]),
            ],
        };
        let data = h.encode().unwrap();
        assert_eq!(&data[..8], b"ZLTA\x00\x02\x00\x00");
        assert_eq!(Header::parse(&data).unwrap(), h);
        assert_eq!(h.creator(), Some(&b"zalt 0.0.1"[..]));

        let mut bad = data.clone();
        bad[5] = 3;
        assert!(
            matches!(Header::parse(&bad), Err(Error::Unsupported(_))),
            "0.3"
        );
        bad[5] = 1;
        assert!(
            matches!(Header::parse(&bad), Err(Error::Unsupported(_))),
            "0.1"
        );
        let mut bad = data.clone();
        bad[6] = 0x02;
        assert!(Header::parse(&bad).is_err(), "reserved flag bit");
        let mut bad = data.clone();
        bad.truncate(data.len() - 1);
        assert!(Header::parse(&bad).is_err(), "truncated TLV");
    }

    fn file_with(entries: &[SeekEntry]) -> Vec<u8> {
        let mut file: Vec<u8> = entries
            .iter()
            .flat_map(|e| core::iter::repeat_n(0u8, e.compressed_size as usize))
            .collect();
        file.extend(encode_seek_table(entries).unwrap());
        file
    }

    fn read(file: &[u8]) -> Result<(Vec<SeekEntry>, u64)> {
        read_seek_table(
            file.len() as u64,
            |at, len| Ok(file[at as usize..at as usize + len].to_vec()),
            1 << 20,
        )
    }

    #[test]
    fn a_seek_table_round_trips_and_must_cover_the_file_exactly() {
        let entries = [
            SeekEntry {
                compressed_size: 40,
                decompressed_size: 0,
                checksum: Some(0),
            },
            SeekEntry {
                compressed_size: 100,
                decompressed_size: 4096,
                checksum: Some(7),
            },
        ];
        let file = file_with(&entries);
        let (got, len) = read(&file).unwrap();
        assert_eq!(got, entries);
        assert_eq!(len, 8 + 2 * 12 + 9);

        // One byte too many at the front: the table no longer covers the file.
        let mut longer = vec![0u8];
        longer.extend_from_slice(&file);
        assert!(read(&longer).is_err(), "§11.1 step 4");
    }

    #[test]
    fn the_seek_table_footer_is_strict() {
        let file = file_with(&[SeekEntry {
            compressed_size: 10,
            decompressed_size: 0,
            checksum: Some(0),
        }]);
        let n = file.len();
        let mut bad = file.clone();
        bad[n - 5] |= 0x04; // a reserved descriptor bit
        assert!(read(&bad).is_err());
        let mut bad = file.clone();
        bad[n - 1] ^= 0xFF; // the seekable magic
        assert!(read(&bad).is_err());
        let mut bad = file.clone();
        bad[n - 9] = 200; // more entries than the frame holds
        assert!(read(&bad).is_err());
        assert!(read(&file[..10]).is_err());
    }
}
