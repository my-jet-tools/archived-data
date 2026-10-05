//! An archive of VARIABLE-SIZE keys, each owning a preallocated page of fixed-size slots.
//!
//! The split archive — [`VariableKeyPreallocatedSlotsArchiveWriter`] and
//! [`VariableKeyPreallocatedSlotsArchiveReader`] — is an INDEX and data files `0 ..= 255`, asked for by
//! role through an [`crate::IoWriterFactory`] / [`crate::IoReaderFactory`]. Little-endian throughout except
//! the key length prefix; every offset is a `u32` inside its own file:
//!
//! ```text
//! index  L0    frame 0 at 0, chained  size u32 · next u32 · count u32 · hash u64   then count × (P from · P to · l1_offset u32)
//! index  L1    anywhere               size u32 · next u32 · count u32              then count × (P key · data_ref), ascending
//! data   page  one per key            size u32 · next u32 · item_len u16           then SLOTS × item_len
//!
//! P        = length prefix (1 byte below 128, else 2 bytes big-endian with the top bit set) · key bytes
//! data_ref = file_no u8 · offset u32
//! ```
//!
//! `size` is the whole frame including its header; `next` is the absolute offset of a continuation frame
//! in the same file, 0 on the last. `hash` is FNV-1a 64 of the L0 header's three fields, checked before
//! anything in the frame is trusted. The entries of an L0 or L1 frame end at a `0x00` prefix (the end
//! marker) or at `size`, whichever comes first, and must be exactly that frame's `count`. Writers chain
//! only L0; readers follow `next` on every frame.
//!
//! Two shapes of the same format exist:
//!
//! * the WORKING archive, built by [`VariableKeyPreallocatedSlotsArchiveWriter`] from keys in any order: L0
//!   frames of `l0_frame_bytes`, L1 pages of `l1_page_bytes` zero-padded past their entries, both appended
//!   to the index as they are needed, data pages appended to the current data file and rolled over to the
//!   next at `max_data_file_bytes`. Its index exists from the moment the writer is created, and opens only
//!   when its last flush COMPLETED and nothing was written to its files after it — otherwise `open` is
//!   `Corrupted`: frame 0's hash does not verify before the first flush completes, is spoiled in place
//!   before the first byte written after one, and verifies again only once the next flush writes frame 0
//!   last;
//! * the COMPACT archive, produced by [`VariableKeyPreallocatedSlotsArchiveReader::compaction_plan`] and
//!   [`VariableKeyPreallocatedSlotsArchiveReader::write_compacted_to`]: ONE L0 frame of exact size, L1
//!   pages of exact size — at most [`COMPACT_L1_MAX_KEYS`] keys and closed before an entry would take them
//!   past [`COMPACT_L1_PAGE_BYTES`] — then data pages in ascending key order across the data files. It
//!   depends only on the keys, the items and `max_data_file_bytes`. The data files are written first and
//!   the index LAST, so a compact archive is complete when its index exists AND opens: a compaction that
//!   failed in its index pass can leave a short index, which `open` refuses.
//!
//! [`VariableKeyPreallocatedSlotsArchiveReader`] reads both shapes: `get` one key, `get_range` every key
//! between two bounds, the L1 pages that lie back to back in the index read together, and so are the data
//! pages that do in a data file. Every key that starts with a prefix is the range up to
//! [`last_key_starting_with`] of it.
//!
//! The `…SingleFile` structures read and write the older ONE-file format through
//! `AsyncRead + AsyncSeek`, with `u64` offsets:
//!
//! ```text
//! L0    frame 0 at 0, chained  size u32 · next u64 · count u32           then count × (P from · P to · offset u64)
//! L1    anywhere               size u32 · count u32                      then count × (P key · offset u64)
//! data  one per key            size u32 · next u64 (0) · item_len u16    then SLOTS × item_len
//! ```
//!
//! Its working file is built by [`VariableKeyPreallocatedSlotsArchiveWriterSingleFile`] and its compact
//! file by [`VariableKeyPreallocatedSlotsArchiveReaderSingleFile::write_compacted_to`]. The pictures of
//! both formats are in this module's `README.md`.

mod entry_walk;
mod key_prefix;
mod key_starting_with;
mod layout;
mod primary_key;
mod reader;
mod reader_compact;
mod reader_range;
mod reader_single_file;
mod reader_single_file_compact;
mod split_layout;
#[cfg(test)]
mod split_test_support;
#[cfg(test)]
mod test_types;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_split_compaction;
#[cfg(test)]
mod tests_split_range;
#[cfg(test)]
mod tests_split_reader;
#[cfg(test)]
mod tests_split_working_index;
#[cfg(test)]
mod tests_split_writer;
#[cfg(test)]
mod tests_split_writer_failures;
mod writer;
mod writer_single_file;

pub(crate) use entry_walk::*;
pub use key_prefix::*;
pub use key_starting_with::*;
pub use layout::*;
pub use primary_key::*;
pub use reader::*;
pub use reader_single_file::*;
pub(crate) use split_layout::*;
pub use writer::*;
pub use writer_single_file::*;
