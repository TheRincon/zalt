//! Decode parameters.
//!
//! These are format-level bounds only. **Policy defaults belong in L2**, not
//! here: this crate's default is the widest the format allows, so that
//! `zalt-codec` on its own behaves like a general zstd implementation rather
//! than like a browser. `zalt-embed` narrows it.

use crate::error::{Error, Result};

/// Largest `windowLog` the format can express.
///
/// `Window_Descriptor` carries a 5-bit exponent and `windowLog = 10 + exponent`,
/// so a spec-legal frame header can request a 2 TiB window. This is exactly the
/// amplification primitive discussed in `doc/ZALT-MAC.md` §6 — the reason a
/// ceiling is mandatory rather than advisory.
pub const WINDOWLOG_FORMAT_MAX: u8 = 41;

/// Largest `windowLog` this implementation will ever accept, matching libzstd's
/// own refusal point on 64-bit.
pub const WINDOWLOG_ABSOLUTE_MAX: u8 = 31;

/// Smallest `windowLog` the format can express.
pub const WINDOWLOG_MIN: u8 = 10;

/// Bounds applied while decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Reject frames declaring a window above this.
    ///
    /// Checked at header parse, before anything is allocated. Rejecting early
    /// matters: a decoder cannot honour a smaller window than declared and hope
    /// for the best, because it would fail partway through once a
    /// long-distance match arrives.
    pub max_window_log: u8,

    /// Reject frames whose declared or actual output exceeds this, if set.
    pub max_decoded_size: Option<u64>,
}

impl Default for DecodeLimits {
    /// The general-purpose default: as permissive as the implementation allows.
    ///
    /// Hosts decoding untrusted input must narrow this. `zalt-embed` does.
    fn default() -> Self {
        Self {
            max_window_log: WINDOWLOG_ABSOLUTE_MAX,
            max_decoded_size: None,
        }
    }
}

impl DecodeLimits {
    /// Check a frame's declared `windowLog` against this policy.
    pub fn check_window_log(&self, requested_log: u8) -> Result<()> {
        let limit = self.max_window_log.min(WINDOWLOG_ABSOLUTE_MAX);
        if requested_log > limit {
            return Err(Error::WindowTooLarge {
                requested_log,
                limit_log: limit,
            });
        }
        Ok(())
    }
}

/// Decode a `Window_Descriptor` byte into a window size in bytes.
///
/// `windowLog = 10 + exponent`, `windowBase = 1 << windowLog`,
/// `windowSize = windowBase + (windowBase / 8) * mantissa`.
pub fn window_size_from_descriptor(descriptor: u8) -> Result<u64> {
    let exponent = u32::from(descriptor >> 3);
    let mantissa = u64::from(descriptor & 0x07);
    let window_log = 10 + exponent;
    if window_log > u32::from(WINDOWLOG_FORMAT_MAX) {
        return Err(Error::Malformed);
    }
    let base = 1u64 << window_log;
    Ok(base + (base / 8) * mantissa)
}

/// `windowLog` implied by a `Window_Descriptor` byte, without computing a size.
///
/// Used for the policy check before any allocation is contemplated.
pub fn window_log_from_descriptor(descriptor: u8) -> u8 {
    10 + (descriptor >> 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_zero_is_1_kib() {
        assert_eq!(window_size_from_descriptor(0).unwrap(), 1024);
        assert_eq!(window_log_from_descriptor(0), 10);
    }

    #[test]
    fn mantissa_adds_eighths() {
        // exponent 0, mantissa 4 -> 1024 + (128 * 4)
        assert_eq!(window_size_from_descriptor(4).unwrap(), 1024 + 512);
    }

    #[test]
    fn max_descriptor_is_enormous() {
        // The whole point: one byte, 2 TiB. Nothing stops a server sending it.
        let d = 0xFF;
        assert_eq!(window_log_from_descriptor(d), 41);
        assert!(window_size_from_descriptor(d).unwrap() > (1 << 41));
    }

    #[test]
    fn default_limits_reject_beyond_implementation_max() {
        let l = DecodeLimits::default();
        assert!(l.check_window_log(31).is_ok());
        assert!(matches!(
            l.check_window_log(41),
            Err(Error::WindowTooLarge { .. })
        ));
    }

    #[test]
    fn narrowed_limits_reject_at_policy_not_format_max() {
        let l = DecodeLimits {
            max_window_log: 23,
            ..Default::default()
        };
        assert!(l.check_window_log(23).is_ok());
        assert!(matches!(
            l.check_window_log(24),
            Err(Error::WindowTooLarge {
                requested_log: 24,
                limit_log: 23
            })
        ));
    }
}
