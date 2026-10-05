use crate::le_u32;

pub(crate) const DATA_REF_LEN: usize = 5;

/// Where a key's data page is: which data file, and the offset inside it.
///
/// ```text
/// file_no u8 · offset u32
/// ```
///
/// Ordered as `(file_no, offset)`, which is file order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DataRef {
    pub file_no: u8,
    pub offset: u32,
}

impl DataRef {
    pub fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(self.file_no);
        out.extend_from_slice(&self.offset.to_le_bytes());
    }

    /// `bytes` holds [`DATA_REF_LEN`] bytes from `at`.
    pub fn decode(bytes: &[u8], at: usize) -> Self {
        Self {
            file_no: bytes[at],
            offset: le_u32(bytes, at + 1),
        }
    }

    /// One `u64` per page across every data file, ordered as the refs are — so a cache keyed by it
    /// flushes files and pages front to back.
    pub fn cache_key(&self) -> u64 {
        ((self.file_no as u64) << 32) | self.offset as u64
    }

    pub fn from_cache_key(key: u64) -> Self {
        Self {
            file_no: (key >> 32) as u8,
            offset: key as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_round_trips_through_its_bytes_and_its_cache_key() {
        let refs = [
            DataRef {
                file_no: 0,
                offset: 0,
            },
            DataRef {
                file_no: 7,
                offset: 0x0102_0304,
            },
            DataRef {
                file_no: 255,
                offset: u32::MAX,
            },
        ];

        let mut bytes = vec![0xAA];
        for data_ref in refs {
            data_ref.encode_to(&mut bytes);
        }
        assert_eq!(bytes.len(), 1 + 3 * DATA_REF_LEN);
        assert_eq!(&bytes[6..11], &[7, 4, 3, 2, 1]);

        for (index, data_ref) in refs.iter().enumerate() {
            assert_eq!(DataRef::decode(&bytes, 1 + index * DATA_REF_LEN), *data_ref);
            assert_eq!(DataRef::from_cache_key(data_ref.cache_key()), *data_ref);
        }
    }

    #[test]
    fn cache_keys_order_as_file_then_offset() {
        let mut refs: Vec<DataRef> = [(2, 0), (0, u32::MAX), (1, 5), (0, 3), (2, 1), (1, 0)]
            .into_iter()
            .map(|(file_no, offset)| DataRef { file_no, offset })
            .collect();

        let mut by_key = refs.clone();
        by_key.sort_by_key(|data_ref| data_ref.cache_key());
        refs.sort();

        assert_eq!(by_key, refs);
    }
}
