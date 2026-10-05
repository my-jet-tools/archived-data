use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rust_extensions::date_time::DateTimeAsMicroseconds;

use crate::{
    ArchiveError, ArchiveFileReader, DATA_FRAME_LEN, DEFAULT_L1_CAPACITY,
    DEFAULT_MAX_DATA_FILE_BYTES, DEFAULT_READER_DATA_CACHE_PAGES, DEFAULT_READER_L1_CACHE_PAGES,
    DEFAULT_READER_L2_CACHE_FRAMES, DataRef, IoReaderFactory, L2_FRAME_LEN, LazyDataReaders,
    MICROSECONDS_PER_DAY, MINUTES_PER_DAY, MinuteCells, PageCache, PrimaryKeyFixedSize, RecordRef,
    RecordsCursor, SizedReader, SplitFrameKind, SplitL0Interval, check_max_data_file_bytes,
    check_records_l1_header, check_records_type_sizes, count_split_l1_keys,
    decode_records_l1_chain, decode_split_l0_chain, minute_of_day, open_split_index,
    read_split_chain, records_l1_page_len, split_data_ref_of, split_l1_page,
};

/// Reads a WORKING or a COMPACT day archive of records through the readers of one factory — local files, or
/// S3 objects read a range per call — and is shared by reference (`Arc`) between concurrent callers.
///
/// The archive does not store its day, so the reader is told it, as the writer was: a record a read meets
/// whose time is not in that day is `Corrupted`, and so is a cell that does not point at the first record of
/// its minute. A read that meets no record cannot tell — told the wrong day, it answers an empty list.
///
/// The index's L0 chain is read once by [`Self::open`] and lives in memory for the reader's life. Data files
/// are opened on first use, once each. L1 pages, L2 frames and data frames are cached, each cache bounded by
/// count under its own `RwLock`, exactly as the split readers cache their pages; a walk reads the frames the
/// cache does not hold through a window of its own, which grows while its key's frames lie back to back — so
/// a compact key's day is a few reads of at most 512 KiB each, not a read a frame. The archive is assumed
/// immutable while it is read.
pub struct FixedKeyMinuteIndexedRecordsArchiveReader<TKey, TFactory, TReader> {
    pub(crate) factory: TFactory,
    day_start: DateTimeAsMicroseconds,
    pub(crate) index: SizedReader<TReader>,
    pub(crate) l0: Vec<SplitL0Interval<TKey>>,
    l0_frames: usize,
    data_files: LazyDataReaders<TReader>,
    l1_cache: PageCache<Vec<(TKey, u32)>>,
    l2_cache: PageCache<MinuteCells>,
    data_cache: PageCache<Vec<u8>>,
    max_data_file_bytes: AtomicU64,
}

impl<TKey, TFactory, TReader> FixedKeyMinuteIndexedRecordsArchiveReader<TKey, TFactory, TReader>
where
    TKey: PrimaryKeyFixedSize,
    TFactory: IoReaderFactory<TReader>,
    TReader: ArchiveFileReader,
{
    /// Opens the index of the archive of the day starting at `day_start` and reads its L0 chain, refusing as
    /// `Corrupted` everything the split readers refuse at open: an L0 header that fails its hash — an index
    /// whose last flush did not complete, or bytes that are not an index — a broken chain, intervals out of
    /// order, and an index that ends before or inside the L1 page at its highest offset. No data file is
    /// opened.
    pub async fn open(
        factory: TFactory,
        day_start: DateTimeAsMicroseconds,
    ) -> Result<Self, ArchiveError> {
        check_records_type_sizes::<TKey>(1, DEFAULT_L1_CAPACITY);

        let (index, l0, l0_frames) = open_split_index(&factory, decode_split_l0_chain).await?;

        Ok(Self {
            factory,
            day_start,
            index,
            l0,
            l0_frames,
            data_files: LazyDataReaders::new(),
            l1_cache: PageCache::new(DEFAULT_READER_L1_CACHE_PAGES),
            l2_cache: PageCache::new(DEFAULT_READER_L2_CACHE_FRAMES),
            data_cache: PageCache::new(DEFAULT_READER_DATA_CACHE_PAGES),
            max_data_file_bytes: AtomicU64::new(DEFAULT_MAX_DATA_FILE_BYTES),
        })
    }

    pub fn day_start(&self) -> DateTimeAsMicroseconds {
        self.day_start
    }

    /// In pages. A shrink evicts down to `pages` before it returns, and the cache stays within it.
    pub async fn set_l1_cache_size(&self, pages: usize) {
        self.l1_cache.resize(pages).await;
    }

    /// In frames. A shrink evicts down to `frames` before it returns, and the cache stays within it.
    pub async fn set_l2_cache_size(&self, frames: usize) {
        self.l2_cache.resize(frames).await;
    }

    /// In frames. A shrink evicts down to `frames` before it returns, and the cache stays within it.
    pub async fn set_data_cache_size(&self, frames: usize) {
        self.data_cache.resize(frames).await;
    }

    /// The data file size the next [`Self::compaction_plan`] lays the compact data files out with;
    /// [`DEFAULT_MAX_DATA_FILE_BYTES`] until set. It says nothing about the archive being read.
    ///
    /// Panics above 4 GiB or below one data frame.
    pub fn set_max_data_file_bytes(&self, bytes: u64) {
        check_max_data_file_bytes(bytes, DATA_FRAME_LEN);
        self.max_data_file_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn max_data_file_bytes(&self) -> u64 {
        self.max_data_file_bytes.load(Ordering::Relaxed)
    }

    pub fn l0_intervals(&self) -> usize {
        self.l0.len()
    }

    /// How many frames the L0 chain was read from; 1 for every compact archive.
    pub fn l0_frames(&self) -> usize {
        self.l0_frames
    }

    /// The sum of the `count` of every L1 frame header: one small read per L1 frame, uncached.
    pub async fn keys_count(&self) -> Result<usize, ArchiveError> {
        count_split_l1_keys(&self.index, &self.l0, check_records_l1_header::<TKey>).await
    }

    /// Every record of `key` whose time `t` is `from <= t <= to`, whole and in time order — records of an
    /// equal time in the order they were written. A key the archive does not hold, `from > to`, and a range
    /// outside the archive's day are an empty list; the last two read nothing.
    ///
    /// The walk starts at the first minute from `from` on that has a record, and ends at the first record
    /// later than `to` or at the end of the key's chain. A failed read or a damaged frame is `Err`, never a
    /// shorter list.
    pub async fn get_range(
        &self,
        key: &TKey,
        from: DateTimeAsMicroseconds,
        to: DateTimeAsMicroseconds,
    ) -> Result<Vec<Vec<u8>>, ArchiveError> {
        let (from, to) = (from.unix_microseconds, to.unix_microseconds);
        let day_start = self.day_start.unix_microseconds;

        let outside_the_day =
            to < day_start || from.saturating_sub(day_start) >= MICROSECONDS_PER_DAY;

        if from > to || outside_the_day {
            return Ok(Vec::new());
        }

        let from_minute = minute_of_day(day_start, from.max(day_start)).unwrap_or(0);

        self.records_of(key, from_minute, |time| time > to, |time| time >= from)
            .await
    }

    /// The minutes of the day, `0 ..= 1439`, in which `key` has at least one record, ascending — read off the
    /// key's L2 frame alone, so no data frame is touched. A key the archive does not hold has none.
    ///
    /// **WHICH minutes, never how much is in them**: a cell names only the FIRST record of its minute. It is
    /// what a reader that serves a day in slices plans them by — so that no slice is a whole day of a key whose
    /// day is too big to hold, while a quiet key's day is still one read.
    pub async fn minutes_with_records(&self, key: &TKey) -> Result<Vec<u16>, ArchiveError> {
        let Some(l2_offset) =
            split_data_ref_of(&self.l0, key, |interval| self.l1_page(interval)).await?
        else {
            return Ok(Vec::new());
        };

        let cells = self.l2_frame(l2_offset).await?;

        Ok((0..MINUTES_PER_DAY)
            .filter(|minute| cells.get(*minute).is_some())
            .map(|minute| minute as u16)
            .collect())
    }

    /// Every record of `key`, whole and in time order. A key the archive does not hold is an empty list.
    pub async fn get_day(&self, key: &TKey) -> Result<Vec<Vec<u8>>, ArchiveError> {
        self.records_of(key, 0, |_| false, |_| true).await
    }

    /// The records of `key` from the first minute from `from_minute` on that has one, until `past_the_end`
    /// says a record is past the range, keeping those `wanted` says are in it.
    async fn records_of(
        &self,
        key: &TKey,
        from_minute: usize,
        past_the_end: impl Fn(i64) -> bool,
        wanted: impl Fn(i64) -> bool,
    ) -> Result<Vec<Vec<u8>>, ArchiveError> {
        let Some(l2_offset) =
            split_data_ref_of(&self.l0, key, |interval| self.l1_page(interval)).await?
        else {
            return Ok(Vec::new());
        };

        let cells = self.l2_frame(l2_offset).await?;

        let Some((minute, first)) = cells.first_from(from_minute) else {
            return Ok(Vec::new());
        };

        let mut cursor = RecordsCursor::new(self, &cells, minute, first, true);
        let mut records = Vec::new();

        while let Some(record) = cursor.next().await? {
            if past_the_end(record.time) {
                break;
            }

            if wanted(record.time) {
                records.push(record.bytes().to_vec());
            }
        }

        Ok(records)
    }

    /// The L1 page of `interval` through the L1 cache, checked to span `interval` whether cached or just
    /// read.
    async fn l1_page(
        &self,
        interval: &SplitL0Interval<TKey>,
    ) -> Result<Arc<Vec<(TKey, u32)>>, ArchiveError> {
        split_l1_page(&self.l1_cache, interval, self.source_l1_page(interval)).await
    }

    /// The L1 page of `interval`, read through no cache.
    pub(crate) async fn source_l1_page(
        &self,
        interval: &SplitL0Interval<TKey>,
    ) -> Result<Vec<(TKey, u32)>, ArchiveError> {
        let frames = read_split_chain(
            &self.index,
            interval.l1_offset as u64,
            SplitFrameKind::L1,
            records_l1_page_len::<TKey>(DEFAULT_L1_CAPACITY),
        )
        .await?;

        decode_records_l1_chain(&frames, interval)
    }

    async fn l2_frame(&self, offset: u32) -> Result<Arc<MinuteCells>, ArchiveError> {
        if let Some(cells) = self.l2_cache.get(offset as u64).await {
            return Ok(cells);
        }

        let cells = self.source_l2_frame(offset).await?;
        Ok(self
            .l2_cache
            .insert_within_capacity(offset as u64, cells)
            .await)
    }

    /// The L2 frame at `offset`, read through no cache.
    pub(crate) async fn source_l2_frame(&self, offset: u32) -> Result<MinuteCells, ArchiveError> {
        let bytes = self.index.read_exact(offset as u64, L2_FRAME_LEN).await?;
        MinuteCells::decode(&bytes, offset as u64)
    }

    /// Data file `no`, opened on first use.
    pub(crate) async fn data_file(&self, no: u8) -> Result<&SizedReader<TReader>, ArchiveError> {
        self.data_files.get(&self.factory, no).await
    }

    /// The frame `at` names, when the data cache holds it: its header and its records.
    pub(crate) async fn cached_data_frame(&self, at: RecordRef) -> Option<Arc<Vec<u8>>> {
        self.data_cache.get(data_cache_key(at)).await
    }

    /// Puts a frame just read into the data cache; a frame somebody inserted meanwhile wins.
    pub(crate) async fn cache_data_frame(&self, at: RecordRef, frame: Vec<u8>) -> Arc<Vec<u8>> {
        self.data_cache
            .insert_within_capacity(data_cache_key(at), frame)
            .await
    }
}

fn data_cache_key(at: RecordRef) -> u64 {
    DataRef {
        file_no: at.file_no,
        offset: at.frame,
    }
    .cache_key()
}
