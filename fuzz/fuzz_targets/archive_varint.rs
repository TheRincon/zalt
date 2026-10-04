//! VarInt (§2.1): anything accepted is the one minimal spelling of its value.
//!
//! The property behind §2.1's three refusals: a decoder that accepted a
//! non-minimal or overlong encoding would accept two different byte strings as
//! the same value, and so two different indexes as the same archive. So every
//! value decoded here is re-encoded and must reproduce exactly the bytes it
//! was read from.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zalt_archive::varint;

fuzz_target!(|data: &[u8]| {
    if let Ok((value, used)) = varint::get(data) {
        let mut again = Vec::new();
        varint::put(&mut again, value);
        assert_eq!(again, &data[..used], "accepted a non-canonical VarInt");
    }
    if let Ok((value, used)) = varint::get_signed(data) {
        let mut again = Vec::new();
        varint::put_signed(&mut again, value);
        assert_eq!(again, &data[..used], "accepted a non-canonical SVarInt");
    }
});
