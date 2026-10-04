//! VarInt and SVarInt (spec §2.1–§2.2): LEB128, strictly.
//!
//! Every field of a Central Index entry rides on this, so it is the first place
//! a hostile index meets the reader. §2.1 asks decoders to reject three things,
//! and each is a different way for one value to have two spellings or none:
//!
//! - **more than ten bytes** — a 64-bit value never needs an eleventh;
//! - **overflow** — the tenth byte may carry one bit, not seven;
//! - **non-minimal encodings** — a final `0x00` group after the first byte,
//!   which spells the same value as the encoding without it.
//!
//! The last one matters beyond tidiness. An index is hashed and compared (§7,
//! `Index_Checksum`), and a reader that accepted two spellings of a value would
//! accept two byte-different indexes describing the same archive — exactly the
//! kind of ambiguity a format meant to be audited should not have.

/// Why a VarInt was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarIntError {
    /// The input ended inside a VarInt.
    Truncated,
    /// More than ten bytes, or a value past `u64::MAX`.
    Overflow,
    /// A redundant trailing zero group: the value has a shorter spelling.
    NonMinimal,
}

impl core::fmt::Display for VarIntError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "truncated VarInt",
            Self::Overflow => "VarInt overflows 64 bits",
            Self::NonMinimal => "non-minimal VarInt",
        })
    }
}

/// Append the minimal LEB128 encoding of `value`.
pub fn put(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let group = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(group);
            return;
        }
        out.push(group | 0x80);
    }
}

/// Append `value` ZigZag-mapped, so small negative numbers stay short.
pub fn put_signed(out: &mut Vec<u8>, value: i64) {
    put(out, ((value << 1) ^ (value >> 63)) as u64);
}

/// Decode one VarInt from the front of `input`, returning it and the bytes
/// consumed.
pub fn get(input: &[u8]) -> Result<(u64, usize), VarIntError> {
    let mut value = 0u64;
    for (i, &byte) in input.iter().enumerate() {
        if i == 10 {
            return Err(VarIntError::Overflow);
        }
        let group = u64::from(byte & 0x7F);
        // The tenth byte holds bits 63 and up; only bit 63 exists.
        if i == 9 && group > 1 {
            return Err(VarIntError::Overflow);
        }
        value |= group << (7 * i);
        if byte & 0x80 == 0 {
            if i > 0 && byte == 0 {
                return Err(VarIntError::NonMinimal);
            }
            return Ok((value, i + 1));
        }
    }
    Err(VarIntError::Truncated)
}

/// Decode one SVarInt.
pub fn get_signed(input: &[u8]) -> Result<(i64, usize), VarIntError> {
    let (zz, used) = get(input)?;
    Ok((((zz >> 1) as i64) ^ -((zz & 1) as i64), used))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, v);
        out
    }

    #[test]
    fn values_round_trip_at_every_group_boundary() {
        let mut probes = vec![0u64, 1, u64::MAX, u64::MAX - 1, 1 << 63];
        for k in 1..64 {
            probes.extend([(1u64 << k) - 1, 1u64 << k, (1u64 << k) + 1]);
        }
        for v in probes {
            let bytes = enc(v);
            assert_eq!(get(&bytes), Ok((v, bytes.len())), "{v}");
            // Shortest possible: ceil(bits / 7), at least one byte.
            let bits = 64 - v.leading_zeros().min(63);
            assert_eq!(bytes.len() as u32, bits.div_ceil(7).max(1), "{v}");
        }
    }

    #[test]
    fn the_three_refusals_of_section_2_1() {
        // Non-minimal: 1 spelled with a redundant zero group.
        assert_eq!(get(&[0x81, 0x00]), Err(VarIntError::NonMinimal));
        assert_eq!(get(&[0x80, 0x80, 0x00]), Err(VarIntError::NonMinimal));
        // A lone zero byte is zero, not non-minimal.
        assert_eq!(get(&[0x00]), Ok((0, 1)));
        // Eleven bytes.
        assert_eq!(get(&[0x80; 11]), Err(VarIntError::Overflow));
        // Ten bytes, but the last carries bits past 63.
        let mut too_big = vec![0xFF; 9];
        too_big.push(0x02);
        assert_eq!(get(&too_big), Err(VarIntError::Overflow));
        // Ten bytes with exactly bit 63: u64::MAX's own spelling.
        assert_eq!(get(&enc(u64::MAX)), Ok((u64::MAX, 10)));
        // Truncated.
        assert_eq!(get(&[0x80]), Err(VarIntError::Truncated));
        assert_eq!(get(&[]), Err(VarIntError::Truncated));
    }

    #[test]
    fn decoding_stops_at_the_first_complete_value() {
        assert_eq!(get(&[0x05, 0xFF, 0xFF]), Ok((5, 1)));
    }

    #[test]
    fn signed_values_zigzag_both_ways() {
        for v in [0i64, 1, -1, 63, -64, 64, -65, i64::MAX, i64::MIN] {
            let mut out = Vec::new();
            put_signed(&mut out, v);
            assert_eq!(get_signed(&out), Ok((v, out.len())), "{v}");
        }
        // Small magnitudes of either sign stay one byte — the point of ZigZag.
        for v in -64..=63i64 {
            let mut out = Vec::new();
            put_signed(&mut out, v);
            assert_eq!(out.len(), 1, "{v}");
        }
    }
}
