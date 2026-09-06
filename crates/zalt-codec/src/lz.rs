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

    /// The dictionary rejection, which no other test reaches.
    ///
    /// Both callers pass `dict_size == 0`, so `available == pos` and the first
    /// bound catches everything — this branch is unreachable in the shipped
    /// decoder and coverage correctly showed it as dead. It stops being dead
    /// the moment M4 loads a dictionary, and what it must do then is refuse
    /// rather than quietly copy the wrong bytes, so the behaviour is worth
    /// pinning now rather than discovering later.
    #[test]
    fn an_offset_reaching_into_a_dictionary_is_refused_until_m4() {
        let mut buf = [0u8; 64];
        // Sixteen bytes of history, a dictionary notionally behind it, and an
        // offset that reaches past the history into that dictionary.
        assert!(matches!(
            copy_match::<Scalar>(&mut buf, 16, 20, 4, 32),
            Err(Error::OffsetTooLarge {
                offset: 20,
                available: 16
            })
        ));
        // Past the dictionary's start as well: the outer bound, already tested
        // elsewhere, still applies and reports the larger availability.
        assert!(matches!(
            copy_match::<Scalar>(&mut buf, 16, 100, 4, 32),
            Err(Error::OffsetTooLarge {
                offset: 100,
                available: 48
            })
        ));
    }

    /// `copy_literals` is public and, at present, called by nothing.
    ///
    /// `Window::write_all` reaches `Backend::copy_literals` directly and takes
    /// its bounds safety from slicing, so this wrapper — which exists to
    /// validate before dispatching — had no caller and no test. Untested public
    /// API on the security boundary is worse than unused public API: a
    /// downstream user of this crate can call it, and nothing said whether its
    /// bound holds.
    #[test]
    fn copy_literals_validates_its_bound() {
        let mut buf = [0u8; 8];
        assert!(copy_literals::<Scalar>(&mut buf, 2, &[1, 2, 3]).is_ok());
        assert_eq!(&buf[2..5], &[1, 2, 3]);

        // Exactly filling the buffer is legal.
        let mut buf = [0u8; 4];
        assert!(copy_literals::<Scalar>(&mut buf, 0, &[9, 9, 9, 9]).is_ok());

        // One past the end is not.
        let mut buf = [0u8; 4];
        assert!(matches!(
            copy_literals::<Scalar>(&mut buf, 2, &[1, 2, 3]),
            Err(Error::TooLarge)
        ));

        // And the offset arithmetic cannot be made to wrap.
        let mut buf = [0u8; 4];
        assert!(matches!(
            copy_literals::<Scalar>(&mut buf, usize::MAX, &[1]),
            Err(Error::TooLarge)
        ));
    }
}
