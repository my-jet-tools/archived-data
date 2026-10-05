//! Archived data: files that are written once, read back by key, and live anywhere — a local file or an
//! object in S3.
//!
//! Generic over what is stored: nothing here knows what a key or an item means. One module per storage
//! shape; see this crate's `README.md`. What every shape shares — the slot and item traits, the data
//! page, the L0 chain framing, the page caches and the IO — lives at the crate root.
//!
//! The split structures read and write an archive as an index and data files `0 ..= 255`, through
//! [`ArchiveFileWriter`] and [`ArchiveFileReader`] handed out by ROLE by an [`IoWriterFactory`] and an
//! [`IoReaderFactory`]; [`LocalFileIo`] is both factories over a local directory. The `…SingleFile`
//! structures still read and write one file through `tokio::io::AsyncRead + AsyncSeek`.

mod append_frontier;
mod archive_file_reader;
mod archive_file_writer;
mod cache_defaults;
mod calendar;
mod compact_data_files_writer;
mod compact_index_writer;
mod compact_l1_page_plan;
mod compaction_plan;
mod data_file_layout;
mod data_page;
mod data_ref;
mod error;
mod finish_split_writers;
mod fixed_key_minute_indexed_records;
mod fixed_key_preallocated_slots;
mod global_day_index;
mod in_flight_guard;
#[cfg(test)]
mod in_memory_io;
mod io_reader_factory;
mod io_writer_factory;
mod l0_chain;
mod lazy_data_readers;
mod le_bytes;
mod local_file_io;
mod page_cache;
mod pascal_string;
mod positioned_sink;
mod preallocated_data_item;
mod preallocated_slot_key;
mod sized_reader;
mod source_io;
mod split_data_page;
mod split_data_runs;
mod split_data_writers;
mod split_frame;
mod split_frame_chain;
mod split_index_writer;
mod split_key_placement;
mod split_l0_chain;
mod split_l0_hash;
mod split_l0_interval;
mod split_limits;
mod split_range_items;
mod split_range_l1_pages;
mod split_reader_index;
mod split_reader_pages;
#[cfg(test)]
mod split_test_frames;
#[cfg(test)]
mod split_test_io;
mod split_working_flush;
mod split_writer_data_pages;
mod split_writer_l1_pages;
#[cfg(test)]
mod test_items;
mod variable_key_preallocated_slots;
mod writer_data_pages;
mod writer_page;
mod writer_page_cache;

pub(crate) use append_frontier::*;
pub use archive_file_reader::*;
pub use archive_file_writer::*;
pub use cache_defaults::*;
pub(crate) use calendar::*;
pub(crate) use compact_data_files_writer::*;
pub(crate) use compact_index_writer::*;
pub(crate) use compact_l1_page_plan::*;
pub use compaction_plan::*;
pub(crate) use data_file_layout::*;
pub(crate) use data_page::*;
pub(crate) use data_ref::*;
pub use error::*;
pub(crate) use finish_split_writers::*;
pub use fixed_key_minute_indexed_records::*;
pub use fixed_key_preallocated_slots::*;
pub use global_day_index::*;
pub(crate) use in_flight_guard::*;
pub use io_reader_factory::*;
pub use io_writer_factory::*;
pub(crate) use l0_chain::*;
pub(crate) use lazy_data_readers::*;
pub(crate) use le_bytes::*;
pub use local_file_io::*;
pub(crate) use page_cache::*;
pub use pascal_string::*;
pub(crate) use positioned_sink::*;
pub use preallocated_data_item::*;
pub use preallocated_slot_key::*;
pub(crate) use sized_reader::*;
pub(crate) use source_io::*;
pub(crate) use split_data_page::*;
pub(crate) use split_data_runs::*;
pub(crate) use split_data_writers::*;
pub(crate) use split_frame::*;
pub(crate) use split_frame_chain::*;
pub(crate) use split_index_writer::*;
pub(crate) use split_key_placement::*;
pub(crate) use split_l0_chain::*;
pub(crate) use split_l0_hash::*;
pub(crate) use split_l0_interval::*;
pub use split_limits::*;
pub(crate) use split_range_items::*;
pub(crate) use split_range_l1_pages::*;
pub(crate) use split_reader_index::*;
pub(crate) use split_reader_pages::*;
#[cfg(test)]
pub(crate) use split_test_frames::*;
pub(crate) use split_working_flush::*;
pub(crate) use split_writer_data_pages::*;
pub(crate) use split_writer_l1_pages::*;
pub use variable_key_preallocated_slots::*;
pub(crate) use writer_data_pages::*;
pub(crate) use writer_page::*;
pub(crate) use writer_page_cache::*;

/// Every `rust` block of `docs/*.md` is compiled and run by `cargo test --doc`, so the docs cannot drift
/// away from the API.
#[cfg(doctest)]
mod docs {
    #[doc = include_str!("../docs/io.md")]
    struct Io;
    #[doc = include_str!("../docs/split-format.md")]
    struct SplitFormat;
    #[doc = include_str!("../docs/lifecycle.md")]
    struct Lifecycle;
    #[doc = include_str!("../docs/fixed-key-preallocated-slots.md")]
    struct FixedKeyPreallocatedSlots;
    #[doc = include_str!("../docs/variable-key-preallocated-slots.md")]
    struct VariableKeyPreallocatedSlots;
    #[doc = include_str!("../docs/single-file.md")]
    struct SingleFile;
    #[doc = include_str!("../docs/fixed-key-minute-indexed-records.md")]
    struct FixedKeyMinuteIndexedRecords;
    #[doc = include_str!("../docs/global-day-index.md")]
    struct GlobalDayIndex;
    #[doc = include_str!("../docs/archiving-a-day.md")]
    struct ArchivingADay;
    #[doc = include_str!("../docs/pascal-strings.md")]
    struct PascalStrings;
    #[doc = include_str!("../docs/errors.md")]
    struct Errors;
    #[doc = include_str!("../docs/internals.md")]
    struct Internals;
}
