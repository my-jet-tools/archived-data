use std::collections::BTreeMap;

use crate::PrimaryKeyFixedSize;
pub use crate::test_items::{Hour, Record, Rng};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Login(pub u64);

impl PrimaryKeyFixedSize for Login {
    const LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(u64::from_le_bytes(src.try_into().unwrap()))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
}

/// The 0.1.18 day object, built from the written-down layout with its literal sizes and none of the
/// crate's code: L0 (16 + 24 × pages) · L1 pages of 8 + 16 × 1024 · data pages of 14 + 24 × 128.
pub fn reference_day(days: &BTreeMap<u64, Vec<Option<Record>>>) -> Vec<u8> {
    const L1_CAPACITY: usize = 1024;
    const L1_PAGE: usize = 8 + 16 * L1_CAPACITY;
    const DATA_PAGE: usize = 14 + 24 * 128;

    let logins: Vec<u64> = days.keys().copied().collect();
    let pages: Vec<&[u64]> = logins.chunks(L1_CAPACITY).collect();

    let l0_len = 16 + 24 * pages.len();
    let data_start = l0_len + pages.len() * L1_PAGE;

    let mut out = Vec::new();
    out.extend_from_slice(&(l0_len as u32).to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());

    for (page, chunk) in pages.iter().enumerate() {
        out.extend_from_slice(&chunk[0].to_le_bytes());
        out.extend_from_slice(&chunk[chunk.len() - 1].to_le_bytes());
        out.extend_from_slice(&((l0_len + page * L1_PAGE) as u64).to_le_bytes());
    }

    for (page, chunk) in pages.iter().enumerate() {
        let start = out.len();
        out.extend_from_slice(&(L1_PAGE as u32).to_le_bytes());
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());

        for (index, login) in chunk.iter().enumerate() {
            let rank = page * L1_CAPACITY + index;
            out.extend_from_slice(&login.to_le_bytes());
            out.extend_from_slice(&((data_start + rank * DATA_PAGE) as u64).to_le_bytes());
        }

        out.resize(start + L1_PAGE, 0);
    }

    for hours in days.values() {
        out.extend_from_slice(&(DATA_PAGE as u32).to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&128u16.to_le_bytes());

        for hour in hours {
            match hour {
                Some(record) => out.extend_from_slice(&record.0),
                None => out.extend_from_slice(&[0xFF; 128]),
            }
        }
    }

    out
}
