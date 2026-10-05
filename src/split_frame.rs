//! The header every frame of the split archive starts with: the same prefix, then its own fields.
//!
//! ```text
//! L0      size u32 · next u32 · count u32 · hash u64   20 bytes
//! L1      size u32 · next u32 · count u32              12 bytes
//! data    size u32 · next u32 · item_len u16           10 bytes
//! ```
//!
//! `size` is the whole frame including its header. `next` is the absolute offset of the continuation
//! frame in the same file, 0 = none. `hash` is [`split_l0_header_hash`] of the three fields before it: the
//! first bytes of an index are checked against it before anything in them is trusted, so a file that is
//! not a split index is refused as `Corrupted` instead of reading as an archive that holds nothing.

use std::fmt;

use crate::{ArchiveError, le_u16, le_u32, le_u64, split_l0_header_hash};

pub(crate) const SPLIT_L0_HEADER_LEN: usize = 20;
pub(crate) const SPLIT_L1_HEADER_LEN: usize = 12;
pub(crate) const SPLIT_DATA_HEADER_LEN: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitFrameKind {
    L0,
    L1,
    Data,
}

impl SplitFrameKind {
    pub fn header_len(self) -> usize {
        match self {
            Self::L0 => SPLIT_L0_HEADER_LEN,
            Self::L1 => SPLIT_L1_HEADER_LEN,
            Self::Data => SPLIT_DATA_HEADER_LEN,
        }
    }
}

impl fmt::Display for SplitFrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::L0 => f.write_str("L0 frame"),
            Self::L1 => f.write_str("L1 frame"),
            Self::Data => f.write_str("data frame"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SplitFrameHeader {
    pub kind: SplitFrameKind,
    pub offset: u64,
    pub size: usize,
    pub next: u32,
    /// `count` of an L0 or L1 frame, `item_len` of a data frame.
    pub field: u32,
}

impl SplitFrameHeader {
    /// The header at the start of `bytes`, which were read from `offset`. Fewer bytes than a header, an L0
    /// header whose hash does not match its fields — checked before any field is used — or a `size`
    /// shorter than a header, is `Corrupted`.
    pub fn decode(bytes: &[u8], offset: u64, kind: SplitFrameKind) -> Result<Self, ArchiveError> {
        let header_len = kind.header_len();

        if bytes.len() < header_len {
            return Err(ArchiveError::Corrupted(format!(
                "the {kind} at {offset} is cut short: {} of its {header_len} header bytes",
                bytes.len()
            )));
        }

        let (size, next) = (le_u32(bytes, 0), le_u32(bytes, 4));

        let field = match kind {
            SplitFrameKind::L0 => {
                let count = le_u32(bytes, 8);

                if le_u64(bytes, 12) != split_l0_header_hash(size, next, count) {
                    return Err(ArchiveError::Corrupted(format!(
                        "the {kind} at {offset} fails its header hash: the file is not a split \
                         archive index, or its L0 header is damaged"
                    )));
                }

                count
            }
            SplitFrameKind::L1 => le_u32(bytes, 8),
            SplitFrameKind::Data => le_u16(bytes, 8) as u32,
        };

        let size = size as usize;

        if size < header_len {
            return Err(ArchiveError::Corrupted(format!(
                "the {kind} at {offset} declares {size} bytes, shorter than its header"
            )));
        }

        Ok(Self {
            kind,
            offset,
            size,
            next,
            field,
        })
    }

    pub fn end(&self) -> u64 {
        self.offset + self.size as u64
    }
}

/// One frame as the file holds it: `body` is everything past the header up to `size`, padding included.
pub(crate) struct SplitFrame {
    pub header: SplitFrameHeader,
    pub body: Vec<u8>,
}

impl SplitFrame {
    /// One frame out of `bytes` read from `offset`. Bytes past its `size` are dropped and its `next` is
    /// not followed.
    pub fn from_bytes(
        mut bytes: Vec<u8>,
        offset: u64,
        kind: SplitFrameKind,
    ) -> Result<Self, ArchiveError> {
        let header = SplitFrameHeader::decode(&bytes, offset, kind)?;

        if bytes.len() < header.size {
            return Err(ArchiveError::Corrupted(format!(
                "the {kind} at {offset} declares {} bytes and is cut short at {}",
                header.size,
                bytes.len()
            )));
        }

        bytes.truncate(header.size);
        bytes.drain(..kind.header_len());

        Ok(Self {
            header,
            body: bytes,
        })
    }
}

/// An L0 frame of `size` bytes: header with its hash, `body`, zeros.
pub(crate) fn encode_split_l0_frame(
    size: usize,
    next: u32,
    count: usize,
    body: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    let (size_field, count_field) = counted_fields(SplitFrameKind::L0, size, count, body)?;

    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&size_field.to_le_bytes());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&count_field.to_le_bytes());
    out.extend_from_slice(&split_l0_header_hash(size_field, next, count_field).to_le_bytes());
    out.extend_from_slice(body);
    out.resize(size, 0);
    Ok(out)
}

/// An L1 frame of `size` bytes: header, `body`, zeros.
pub(crate) fn encode_split_l1_frame(
    size: usize,
    next: u32,
    count: usize,
    body: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    let (size_field, count_field) = counted_fields(SplitFrameKind::L1, size, count, body)?;

    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&size_field.to_le_bytes());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&count_field.to_le_bytes());
    out.extend_from_slice(body);
    out.resize(size, 0);
    Ok(out)
}

fn counted_fields(
    kind: SplitFrameKind,
    size: usize,
    count: usize,
    body: &[u8],
) -> Result<(u32, u32), ArchiveError> {
    if kind.header_len() + body.len() > size {
        return Err(ArchiveError::ContractViolation(format!(
            "an {kind} of {size} bytes cannot hold {} bytes of entries",
            body.len()
        )));
    }

    let (Ok(size_field), Ok(count_field)) = (u32::try_from(size), u32::try_from(count)) else {
        return Err(ArchiveError::ContractViolation(format!(
            "an {kind} of {size} bytes and {count} entries does not fit its u32 fields"
        )));
    };

    Ok((size_field, count_field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{data_frame, l0_frame, l1_frame};

    fn corruption(result: Result<SplitFrame, ArchiveError>) -> String {
        match result {
            Err(ArchiveError::Corrupted(message)) => message,
            Err(other) => panic!("not Corrupted: {other}"),
            Ok(frame) => panic!("accepted: {:?}", frame.header),
        }
    }

    #[test]
    fn an_l1_frame_round_trips_with_its_padding() {
        let bytes = encode_split_l1_frame(20, 0x0102_0304, 2, &[9, 9, 9]).unwrap();
        assert_eq!(bytes, l1_frame(20, 0x0102_0304, 2, &[9, 9, 9]));

        let mut read = bytes.clone();
        read.extend_from_slice(&[7; 5]);
        let frame = SplitFrame::from_bytes(read, 40, SplitFrameKind::L1).unwrap();

        assert_eq!(
            frame.header,
            SplitFrameHeader {
                kind: SplitFrameKind::L1,
                offset: 40,
                size: 20,
                next: 0x0102_0304,
                field: 2,
            }
        );
        assert_eq!(frame.header.end(), 60);
        assert_eq!(frame.body, [9, 9, 9, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn an_l0_frame_carries_the_hash_of_its_fields_and_round_trips() {
        let bytes = encode_split_l0_frame(28, 0x0102_0304, 2, &[9, 9, 9]).unwrap();
        assert_eq!(bytes, l0_frame(28, 0x0102_0304, 2, &[9, 9, 9]));
        assert_eq!(le_u64(&bytes, 12), split_l0_header_hash(28, 0x0102_0304, 2));

        let frame = SplitFrame::from_bytes(bytes, 0, SplitFrameKind::L0).unwrap();
        assert_eq!(
            (frame.header.size, frame.header.next, frame.header.field),
            (28, 0x0102_0304, 2)
        );
        assert_eq!(frame.body, [9, 9, 9, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn flipping_any_bit_of_an_l0_header_fails_its_hash_before_any_field_is_used() {
        let frame = l0_frame(28, 64, 2, &[1; 8]);

        for bit in 0..SPLIT_L0_HEADER_LEN * 8 {
            let mut bytes = frame.clone();
            bytes[bit / 8] ^= 1 << (bit % 8);

            let message = corruption(SplitFrame::from_bytes(bytes, 0, SplitFrameKind::L0));
            assert!(
                message.contains("not a split archive index"),
                "bit {bit}: {message}"
            );
        }
    }

    #[test]
    fn a_header_of_another_format_is_not_an_l0_frame() {
        // What a SingleFile compact object with no keys starts with: size u32 · next u64 · count u32.
        let mut single_file = 16u32.to_le_bytes().to_vec();
        single_file.extend_from_slice(&[0; 12]);
        single_file.extend_from_slice(&[0; 8]);

        let garbage: Vec<u8> = (0..64u8).map(|n| n.wrapping_mul(37) ^ 0x5A).collect();

        for bytes in [single_file, vec![0; 64], garbage] {
            let message = corruption(SplitFrame::from_bytes(bytes, 0, SplitFrameKind::L0));
            assert!(message.contains("not a split archive index"), "{message}");
        }
    }

    #[test]
    fn a_data_header_carries_a_u16_item_len() {
        let bytes = data_frame(14, 0, 0xBEEF, &[1, 2, 3, 4]);
        let frame = SplitFrame::from_bytes(bytes, 0, SplitFrameKind::Data).unwrap();

        assert_eq!(frame.header.field, 0xBEEF);
        assert_eq!(frame.body, [1, 2, 3, 4]);
    }

    #[test]
    fn a_frame_cut_short_or_shorter_than_its_header_is_corrupted() {
        let cases = [
            (vec![20, 0, 0, 0, 0], SplitFrameKind::L0),
            (l0_frame(20, 0, 0, &[])[..19].to_vec(), SplitFrameKind::L0),
            (l0_frame(19, 0, 0, &[]), SplitFrameKind::L0),
            (l1_frame(11, 0, 1, &[]), SplitFrameKind::L1),
            (data_frame(9, 0, 4, &[]), SplitFrameKind::Data),
            (l1_frame(20, 0, 1, &[])[..19].to_vec(), SplitFrameKind::L1),
        ];

        for (bytes, kind) in cases {
            corruption(SplitFrame::from_bytes(bytes, 0, kind));
        }
    }

    #[test]
    fn an_encoded_frame_must_fit_its_entries_and_its_fields() {
        for (size, count, body) in [(22, 1, vec![0; 3]), (1 << 32, 1, vec![])] {
            assert!(matches!(
                encode_split_l0_frame(size, 0, count, &body),
                Err(ArchiveError::ContractViolation(_))
            ));
        }

        for (size, count, body) in [(14, 1, vec![0; 3]), (1 << 32, 1, vec![])] {
            assert!(matches!(
                encode_split_l1_frame(size, 0, count, &body),
                Err(ArchiveError::ContractViolation(_))
            ));
        }
    }
}
