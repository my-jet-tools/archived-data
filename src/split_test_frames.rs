//! Split-format frames built by hand, with none of the crate's encoders, so a test can put bytes into a
//! file that no writer would produce: continuations, bad sizes, bad links. Every frame is zero-padded up
//! to `size`, and left as it is when its body is already longer. The L0 header hash is worked out here
//! too, from the written-down rule, so a frame built here checks the crate's hash rather than repeating it.
//! Beside them, the little-endian patches and reads a test makes straight on a file's bytes.

use crate::{PreallocatedDataItem, PreallocatedSlotKey};

/// `size u32 · next u32 · count u32 · hash u64`, then `body`.
pub fn l0_frame(size: u32, next: u32, count: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&l0_hash(size, next, count).to_le_bytes());
    out.extend_from_slice(body);
    pad(out, size)
}

/// FNV-1a 64 (offset basis `0xcbf29ce484222325`, prime `0x100000001b3`) over
/// `archived-data/split/L0/v1`, then `size`, `next` and `count` little-endian.
pub fn l0_hash(size: u32, next: u32, count: u32) -> u64 {
    let mut bytes = b"archived-data/split/L0/v1".to_vec();
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&next.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());

    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ *byte as u64).wrapping_mul(0x100_0000_01b3)
    })
}

/// Rewrites the hash of the L0 header at `at` to match its fields, so a test that patches `size`, `next`
/// or `count` reaches the check it is about instead of stopping at the hash.
pub fn restamp_l0(bytes: &mut [u8], at: usize) {
    let field =
        |offset: usize| u32::from_le_bytes(bytes[at + offset..at + offset + 4].try_into().unwrap());
    let hash = l0_hash(field(0), field(4), field(8));
    bytes[at + 12..at + 20].copy_from_slice(&hash.to_le_bytes());
}

/// Patches the `u32` at `field` of the L0 header at `at`, and restamps the header's hash.
pub fn patch_l0_u32(bytes: &mut [u8], at: usize, field: usize, value: u32) {
    patch_u32(bytes, at + field, value);
    restamp_l0(bytes, at);
}

pub fn l1_frame(size: u32, next: u32, count: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(body);
    pad(out, size)
}

pub fn data_frame(size: u32, next: u32, item_len: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&item_len.to_le_bytes());
    out.extend_from_slice(body);
    pad(out, size)
}

pub fn patch_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn patch_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn patch_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn le_u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

pub fn le_u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The offsets of every L0 frame of an index, walked straight off its bytes.
pub fn frame_offsets(index: &[u8]) -> Vec<usize> {
    let mut offsets = vec![0usize];

    loop {
        let next = le_u32_at(index, offsets[offsets.len() - 1] + 4) as usize;
        if next == 0 {
            return offsets;
        }
        offsets.push(next);
    }
}

fn pad(mut out: Vec<u8>, size: u32) -> Vec<u8> {
    if out.len() < size as usize {
        out.resize(size as usize, 0);
    }
    out
}

/// Three slots: a data page small enough to write out by hand.
pub struct TinySlot(pub usize);

impl PreallocatedSlotKey for TinySlot {
    const SLOTS: usize = 3;

    fn slot_index(&self) -> usize {
        self.0
    }
}

pub struct ZeroSlots;

impl PreallocatedSlotKey for ZeroSlots {
    const SLOTS: usize = 0;

    fn slot_index(&self) -> usize {
        0
    }
}

/// Four bytes, all 0xEE when empty.
#[derive(Debug, Clone, PartialEq)]
pub struct TinyItem(pub [u8; 4]);

impl Default for TinyItem {
    fn default() -> Self {
        Self([0xEE; 4])
    }
}

impl PreallocatedDataItem for TinyItem {
    const ITEM_LEN: usize = 4;

    fn from_slice(src: &[u8]) -> Self {
        Self(src.try_into().unwrap())
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }
}
