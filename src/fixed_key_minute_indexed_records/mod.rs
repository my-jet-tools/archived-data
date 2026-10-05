//! A day archive of RECORDS: fixed-size keys, each owning the variable-length records of one day in time
//! order, with a minute index.
//!
//! [`FixedKeyMinuteIndexedRecordsArchiveWriter`] and [`FixedKeyMinuteIndexedRecordsArchiveReader`] read and
//! write it as an INDEX and data files `0 ..= 255`, asked for by role through an [`crate::IoWriterFactory`] /
//! [`crate::IoReaderFactory`]. Little-endian throughout; every offset is a `u32` inside its own file:
//!
//! ```text
//! index  L0     frame 0 at 0, chained  size u32 · next u32 · count u32 · hash u64   then count × (from K · to K · l1_offset u32)
//! index  L1     anywhere               size u32 · next u32 · count u32              then count × (key K · l2_offset u32), ascending
//! index  L2     one per key            size u32 (12 968) · next u32 (0)             then 1440 × (file u8 · frame_start u32 · offset u32)
//! data   frame  a chain per key        size u32 · next u32 · used u16               then used bytes of records
//!
//! record = len u16 · len bytes, the first 8 the event's time: i64 of microseconds
//! ```
//!
//! L0 and L1 are the split archive's. A cell of L2 points at the FIRST record of its minute — the data
//! file, the frame's offset in it, the record's offset from the frame's start — and an empty minute is nine
//! `0xFF` bytes, read as empty by its `offset` alone. `next` of a data frame is an offset in the SAME data
//! file, so a key's chain never leaves its file.
//!
//! Two shapes of the same format exist:
//!
//! * the WORKING archive, built by [`FixedKeyMinuteIndexedRecordsArchiveWriter`] from records in any order:
//!   every data frame 32 KiB in data file 0, frames halved and chained as they fill, L1 pages and L2 frames
//!   appended to the index as they are needed. It opens only when its last flush completed and nothing was
//!   written after it;
//! * the COMPACT archive, produced by [`FixedKeyMinuteIndexedRecordsArchiveReader::compaction_plan`] and
//!   [`FixedKeyMinuteIndexedRecordsArchiveReader::write_compacted_to`]: every key's records one run of frames
//!   cut to their content, a data file ending only between keys; ONE L0 frame, the L2 frames, then the L1
//!   pages. The data files are written first and the index LAST.
//!
//! The byte layout is specified in the Task manager document `architecture/archived-data-history-records.md`
//! of project RMS; the decisions it left open are in `docs/fixed-key-minute-indexed-records.md`.

mod compact_records_data_files;
mod compact_records_index;
mod compact_records_run;
mod layout;
mod minute_cells;
mod reader;
mod reader_compact;
mod reader_keys;
mod record_insert;
mod records_compaction_plan;
mod records_cursor;
mod records_frame;
mod records_read_ahead;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests_compaction;
#[cfg(test)]
mod tests_overflow;
#[cfg(test)]
mod tests_read_ahead;
#[cfg(test)]
mod tests_reader;
#[cfg(test)]
mod tests_working_index;
#[cfg(test)]
mod tests_writer;
mod writer;
mod writer_data_frames;
mod writer_index_pages;
mod writer_insert;
mod writer_key;

pub(crate) use compact_records_data_files::*;
pub(crate) use compact_records_index::*;
pub(crate) use compact_records_run::*;
pub use layout::*;
pub(crate) use minute_cells::*;
pub use reader::*;
pub(crate) use record_insert::*;
pub use records_compaction_plan::*;
pub(crate) use records_cursor::*;
pub(crate) use records_frame::*;
pub(crate) use records_read_ahead::*;
pub use writer::*;
pub(crate) use writer_data_frames::*;
pub(crate) use writer_index_pages::*;
