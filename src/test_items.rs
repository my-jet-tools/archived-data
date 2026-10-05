//! The slot, the item and the randomness every module's tests write with, whatever the key: a day of 24
//! hourly slots of a 128-byte record, and a deterministic xorshift — and the login the modules written after
//! them key their tests with.

use crate::{PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyFixedSize};

/// An account: a `u64`, 8 bytes little-endian.
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

/// `10 + 24 × 128`: a split data page of [`Hour`] slots of [`Record`].
pub const DATA_PAGE: usize = 3082;
/// The data pages a 4 GiB data file holds.
pub const DEFAULT_PAGES_PER_FILE: usize = (1usize << 32) / DATA_PAGE;

#[derive(Debug, Clone, Copy)]
pub struct Hour(pub usize);

impl PreallocatedSlotKey for Hour {
    const SLOTS: usize = 24;

    fn slot_index(&self) -> usize {
        self.0
    }
}

/// Four o/h/l/c figures, all-0xFF when empty.
#[derive(Debug, Clone, PartialEq)]
pub struct Record(pub [u8; 128]);

impl Default for Record {
    fn default() -> Self {
        Self([0xFF; 128])
    }
}

impl Record {
    pub fn with(seed: u64) -> Self {
        let mut record = Self::default();
        for field in 0..16 {
            let value = (seed as f64) * 1.5 + field as f64;
            record.0[field * 8..field * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        record
    }
}

impl PreallocatedDataItem for Record {
    const ITEM_LEN: usize = 128;

    fn from_slice(src: &[u8]) -> Self {
        Self(src.try_into().unwrap())
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }
}

/// A deterministic xorshift, so the tests need no crate for randomness.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

/// A generator whose state is never zero, from any seed.
pub fn seeded(seed: u64) -> Rng {
    Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
}
