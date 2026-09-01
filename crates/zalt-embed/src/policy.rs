//! Decode policy for untrusted input.

use zalt_codec::params::DecodeLimits;

/// Window ceiling applied to untrusted network input, as a `windowLog`.
///
/// `windowLog` 23 is 8 MiB. The number is an **ecosystem** observation, not a
/// security constant and not a compatibility requirement: RFC 8878 names 8 MB
/// as the interop floor decoders should support, and published browser caps
/// have made it the de facto ceiling origins already target. Below it, real
/// content is rejected; above it, nothing additional arrives while the worst
/// case grows.
///
/// Security says *have a ceiling*; it does not pick the number. So this is a
/// default in a struct, not a constant in the codec — see `doc/ZALT-MAC.md` §6.
pub const UNTRUSTED_MAX_WINDOW_LOG: u8 = 23;

/// How a host wants untrusted frames handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Format-level bounds handed to the codec.
    pub limits: DecodeLimits,
    /// Ceiling on total live decoder memory across all instances.
    ///
    /// A per-frame window cap is **not sufficient**: it is per instance, so a
    /// page referencing a hundred compressed subresources decoded concurrently
    /// authorises a hundred times the cap. See [`crate::budget`].
    pub total_memory: u64,
}

impl Policy {
    /// Policy for input arriving from the network.
    pub fn untrusted() -> Self {
        Self {
            limits: DecodeLimits {
                max_window_log: UNTRUSTED_MAX_WINDOW_LOG,
                max_decoded_size: None,
            },
            total_memory: 256 * 1024 * 1024,
        }
    }

    /// Policy for input we produced ourselves, such as a local cache.
    ///
    /// A trusted producer is not an amplification risk, so the window ceiling
    /// serves no purpose here; the total budget still does, because bugs exist.
    pub fn trusted() -> Self {
        Self {
            limits: DecodeLimits::default(),
            total_memory: 1024 * 1024 * 1024,
        }
    }
}

impl Default for Policy {
    /// Defaults to [`Policy::untrusted`].
    ///
    /// The safe default is the restrictive one: a host that forgets to choose
    /// gets the policy appropriate to hostile input, and a host that wants the
    /// permissive one has to say so.
    fn default() -> Self {
        Self::untrusted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_rejects_above_8mib() {
        let p = Policy::untrusted();
        assert!(p.limits.check_window_log(23).is_ok());
        assert!(p.limits.check_window_log(24).is_err());
    }

    #[test]
    fn untrusted_rejects_the_amplification_header() {
        // One byte of frame header, 2 TiB requested.
        assert!(Policy::untrusted().limits.check_window_log(41).is_err());
    }

    #[test]
    fn trusted_still_refuses_beyond_implementation_max() {
        assert!(Policy::trusted().limits.check_window_log(41).is_err());
    }

    #[test]
    fn default_is_the_restrictive_one() {
        assert_eq!(Policy::default(), Policy::untrusted());
    }
}
