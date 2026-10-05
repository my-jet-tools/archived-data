//! The hash that closes every split L0 frame header, and the only identity the split format has.
//!
//! ```text
//! L0 header   size u32 · next u32 · count u32 · hash u64      20 bytes
//! hash        FNV-1a 64 over SPLIT_L0_HASH_FORMAT, then size, next and count as their little-endian bytes
//! ```
//!
//! FNV-1a is written out here rather than taken from `std`, whose `DefaultHasher` may change its algorithm
//! between Rust versions: a stored index must verify under every compiler that will ever read it.

/// Hashed in front of the header fields, so a header of another format — whatever it stores in the same
/// bytes — does not verify.
pub(crate) const SPLIT_L0_HASH_FORMAT: &[u8] = b"archived-data/split/L0/v1";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    fnv1a_64_continue(FNV_OFFSET_BASIS, bytes)
}

fn fnv1a_64_continue(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    hash
}

pub(crate) fn split_l0_header_hash(size: u32, next: u32, count: u32) -> u64 {
    let mut hash = fnv1a_64(SPLIT_L0_HASH_FORMAT);

    for field in [size, next, count] {
        hash = fnv1a_64_continue(hash, &field.to_le_bytes());
    }

    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_64_gives_the_published_answers() {
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn the_header_hash_is_fnv1a_64_of_the_format_then_the_three_fields() {
        let mut bytes = SPLIT_L0_HASH_FORMAT.to_vec();
        bytes.extend_from_slice(&[20, 0, 0, 0, 0x04, 0x03, 0x02, 0x01, 7, 0, 0, 0]);

        assert_eq!(split_l0_header_hash(20, 0x0102_0304, 7), fnv1a_64(&bytes));
        // Pinned: a change to the format constant or to the field order is a change of format.
        assert_eq!(split_l0_header_hash(20, 0, 0), 0x5b5c_3270_5b03_98ca);
    }

    #[test]
    fn every_field_moves_the_hash() {
        let base = split_l0_header_hash(64, 0, 3);

        for other in [
            split_l0_header_hash(65, 0, 3),
            split_l0_header_hash(64, 1, 3),
            split_l0_header_hash(64, 0, 4),
            split_l0_header_hash(3, 0, 64),
        ] {
            assert_ne!(base, other);
        }
    }
}
