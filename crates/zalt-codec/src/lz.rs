//! LZ77 sequence execution: the validated entry point to the backend kernels.
//!
//! Everything here is safe and checked. The backends below it are permitted to
//! skip checks precisely because this layer performed them, so **this is the
//! only route by which a [`Backend`] may be invoked** during decoding.

use crate::backend::Backend;
use crate::error::{Error, Result};

/// Execute one match copy, validating the offset against what is addressable.
///
/// `dict_size` is the number of dictionary bytes logically preceding
/// `buf[0]`; pass 0 when decoding without a dictionary.
///
/// Returns [`Error::OffsetTooLarge`] rather than panicking, because a bad
/// offset is a normal consequence of hostile input, not a bug.
#[inline]
pub fn copy_match<B: Backend>(
    buf: &mut [u8],
    pos: usize,
    offset: usize,
    len: usize,
    dict_size: usize,
) -> Result<()> {
    if offset == 0 {
        return Err(Error::Malformed);
    }
    // An offset may reach back into the dictionary, but never past its start.
    let available = pos + dict_size;
    if offset > available {
        return Err(Error::OffsetTooLarge {
            offset: offset as u64,
            available: available as u64,
        });
    }
    // Reaching into a dictionary is a distinct copy path; not yet implemented
    // (M4). Until then, an offset that would require it is rejected rather
    // than silently mishandled.
    if offset > pos {
        return Err(Error::OffsetTooLarge {
            offset: offset as u64,
            available: pos as u64,
        });
    }
    if pos + len > buf.len() {
        return Err(Error::TooLarge);
    }

    B::copy_match(buf, pos, offset, len);
    Ok(())
}

/// Copy literals into the output, validating lengths.
#[inline]
pub fn copy_literals<B: Backend>(buf: &mut [u8], pos: usize, literals: &[u8]) -> Result<()> {
    let end = pos.checked_add(literals.len()).ok_or(Error::TooLarge)?;
    if end > buf.len() {
        return Err(Error::TooLarge);
    }
    B::copy_literals(&mut buf[pos..end], literals);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Scalar;

    #[test]
    fn non_overlapping_match() {
        let mut buf = [0u8; 16];
        buf[..4].copy_from_slice(b"abcd");
        copy_match::<Scalar>(&mut buf, 4, 4, 4, 0).unwrap();
        assert_eq!(&buf[..8], b"abcdabcd");
    }

    #[test]
    fn overlapping_match_repeats_the_pattern() {
        // The case memmove gets wrong: offset 1 must smear, not copy.
        let mut buf = [0u8; 16];
        buf[0] = b'x';
        copy_match::<Scalar>(&mut buf, 1, 1, 7, 0).unwrap();
        assert_eq!(&buf[..8], b"xxxxxxxx");
    }

    #[test]
    fn overlapping_match_period_three() {
        let mut buf = [0u8; 16];
        buf[..3].copy_from_slice(b"abc");
        copy_match::<Scalar>(&mut buf, 3, 3, 8, 0).unwrap();
        assert_eq!(&buf[..11], b"abcabcabcab");
    }

    #[test]
    fn offset_past_start_is_rejected() {
        let mut buf = [0u8; 16];
        assert!(matches!(
            copy_match::<Scalar>(&mut buf, 2, 5, 1, 0),
            Err(Error::OffsetTooLarge { .. })
        ));
    }

    #[test]
    fn zero_offset_is_rejected() {
        let mut buf = [0u8; 16];
        assert!(matches!(
            copy_match::<Scalar>(&mut buf, 2, 0, 1, 0),
            Err(Error::Malformed)
        ));
    }

    #[test]
    fn overrunning_the_buffer_is_rejected() {
        let mut buf = [0u8; 16];
        buf[0] = 1;
        assert!(matches!(
            copy_match::<Scalar>(&mut buf, 1, 1, 100, 0),
            Err(Error::TooLarge)
        ));
    }
}
