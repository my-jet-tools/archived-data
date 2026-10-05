use std::collections::BTreeMap;

use crate::PrimaryKeyVariableSize;
pub use crate::test_items::{Hour, Record, Rng};

/// A key of any length; sorts as its bytes do.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(pub Vec<u8>);

impl PrimaryKeyVariableSize for Name {
    fn from_slice(src: &[u8]) -> Self {
        Self(src.to_vec())
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }
}

pub type Names = BTreeMap<Vec<u8>, Vec<Option<Record>>>;

/// The compact file built from the written-down layout with its literal sizes and none of the crate's
/// code: one L0 frame (16 + Σ P from · P to · 8) · L1 pages of 8 + Σ (P key · 8), each closed at 1024
/// keys or before 16 KiB · data pages of 14 + 24 × 128.
pub fn reference_compact(names: &Names) -> Vec<u8> {
    const MAX_KEYS: usize = 1024;
    const PAGE_BYTES: usize = 16 * 1024;
    const DATA_PAGE: usize = 14 + 24 * 128;

    fn prefixed(key: &[u8]) -> Vec<u8> {
        let mut out = if key.len() < 128 {
            vec![key.len() as u8]
        } else {
            (key.len() as u16 | 0x8000).to_be_bytes().to_vec()
        };
        out.extend_from_slice(key);
        out
    }

    let mut pages: Vec<(Vec<&[u8]>, usize)> = Vec::new();

    for key in names.keys() {
        let entry = prefixed(key).len() + 8;

        match pages.last_mut() {
            Some((keys, len)) if keys.len() < MAX_KEYS && *len + entry <= PAGE_BYTES => {
                keys.push(key);
                *len += entry;
            }
            _ => pages.push((vec![key], 8 + entry)),
        }
    }

    let l0_len = 16
        + pages
            .iter()
            .map(|(keys, _)| prefixed(keys[0]).len() + prefixed(keys[keys.len() - 1]).len() + 8)
            .sum::<usize>();
    let data_start = l0_len + pages.iter().map(|(_, len)| *len).sum::<usize>();

    let mut out = Vec::new();
    out.extend_from_slice(&(l0_len as u32).to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());

    let mut l1_offset = l0_len;
    for (keys, len) in &pages {
        out.extend_from_slice(&prefixed(keys[0]));
        out.extend_from_slice(&prefixed(keys[keys.len() - 1]));
        out.extend_from_slice(&(l1_offset as u64).to_le_bytes());
        l1_offset += len;
    }

    let mut rank = 0;
    for (keys, len) in &pages {
        out.extend_from_slice(&(*len as u32).to_le_bytes());
        out.extend_from_slice(&(keys.len() as u32).to_le_bytes());

        for key in keys {
            out.extend_from_slice(&prefixed(key));
            out.extend_from_slice(&((data_start + rank * DATA_PAGE) as u64).to_le_bytes());
            rank += 1;
        }
    }

    for hours in names.values() {
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
