//! The length prefix in front of every key.
//!
//! ```text
//! len < 128     one byte:   len
//! len >= 128    two bytes:  (len | 0x8000) BIG-endian
//! ```
//!
//! Big-endian against the rest of the file on purpose: the flag has to be in the FIRST byte read. A
//! single `0x00` is the end marker of the entries of a frame or a page.

use crate::ArchiveError;

pub const MAX_VARIABLE_KEY_LEN: usize = 0x7FFF;

const TWO_BYTES: u8 = 0x80;

pub(crate) fn key_prefix_len(len: usize) -> usize {
    if len < TWO_BYTES as usize { 1 } else { 2 }
}

/// `len` 0 writes the end marker. `len` is at most [`MAX_VARIABLE_KEY_LEN`].
pub(crate) fn encode_key_prefix(len: usize, out: &mut Vec<u8>) {
    if len < TWO_BYTES as usize {
        out.push(len as u8);
    } else {
        out.extend_from_slice(&(len as u16 | 0x8000).to_be_bytes());
    }
}

/// The prefix at `bytes[at]`, which must exist: `None` at the end marker, else `(len, prefix_len)`.
/// A two-byte form of a length below 128 is refused — every length has exactly one spelling, so the
/// compact bytes of a set of keys are unique.
pub(crate) fn decode_key_prefix(
    bytes: &[u8],
    at: usize,
) -> Result<Option<(usize, usize)>, ArchiveError> {
    let first = bytes[at];

    if first & TWO_BYTES == 0 {
        return Ok((first != 0).then_some((first as usize, 1)));
    }

    let Some(second) = bytes.get(at + 1) else {
        return Err(ArchiveError::Corrupted(format!(
            "a two-byte key length at {at} is cut short"
        )));
    };

    let len = (((first & !TWO_BYTES) as usize) << 8) | *second as usize;

    if len < TWO_BYTES as usize {
        return Err(ArchiveError::Corrupted(format!(
            "the key length {len} at {at} is spelled in two bytes"
        )));
    }

    Ok(Some((len, 2)))
}
