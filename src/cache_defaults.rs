pub const DEFAULT_READER_L1_CACHE_PAGES: usize = 10;
pub const DEFAULT_READER_DATA_CACHE_PAGES: usize = 10;
/// The minute index of one key's day, in the archives of records.
pub const DEFAULT_READER_L2_CACHE_FRAMES: usize = 10;

pub const DEFAULT_WRITER_L1_CACHE_PAGES: usize = 64;
pub const DEFAULT_WRITER_DATA_CACHE_PAGES: usize = 1024;
/// About 13 MiB of minute indexes: one frame per key written that day.
pub const DEFAULT_WRITER_L2_CACHE_FRAMES: usize = 1024;
