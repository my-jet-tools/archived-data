//! The GLOBAL DAY INDEX of an MT server: for every login, the days it has data on, so a read over a period
//! opens only the day archives that hold something.
//!
//! One file, never split and never changed in place: [`GlobalDayIndex`] builds a whole new version in memory —
//! the old one plus a day's logins — and serializes it; [`GlobalDayIndexReader`] answers
//! `days_with_data` from a file where it lies, reading one L1 page and one login's blocks.
//!
//! ```text
//! sync time   i64 LE of microseconds            8 bytes
//! L0          the split L0 frame, at offset 8   size u32 · next u32 · count u32 · hash u64, then count × (from K · to K · l1_offset u32)
//! L1          split L1 frames                   size u32 · next u32 · count u32, then count × (login K · data_offset u32 · blocks u16)
//! data        year blocks, ascending years      year u16 · 46 bytes: day d of a leap-year calendar is bit 1 << (d % 8) of byte d / 8
//! ```
//!
//! The format is specified in the Task manager document `architecture/archived-data-history-records.md` of
//! project RMS; the decisions it left open, and what the archiver owns — the temporary name, the rename, the
//! upload — are in this crate's `README.md`.

mod bytes_file_reader;
mod index;
mod layout;
mod reader;
#[cfg(test)]
mod tests;
mod year_days;

pub(crate) use bytes_file_reader::*;
pub use index::*;
pub use layout::*;
pub use reader::*;
pub(crate) use year_days::*;
