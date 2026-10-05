use std::sync::Arc;

use rust_extensions::date_time::DateTimeAsMicroseconds;

use crate::{
    ArchiveError, ArchiveFileReader, COMPACT_L1_CAPACITY, DAY_INDEX_L0_AT,
    DEFAULT_READER_L1_CACHE_PAGES, GLOBAL_DAY_INDEX_SYNC_TIME_LEN, LoginDays, MIN_DAY_INDEX_LEN,
    PageCache, PrimaryKeyFixedSize, SizedReader, SplitFrameKind, SplitL0Interval, YEAR_BLOCK_LEN,
    YearDays, check_day_index_l1_header, count_split_l1_keys, day_index_l1_page_len, days_between,
    decode_day_index_l1_chain, decode_split_l0_chain, decode_year_blocks, read_split_chain,
    refuse_an_index_cut_inside_its_last_l1_page, refuse_l1_offsets_past_the_index,
    split_data_ref_of, split_l1_page,
};

/// How much of the file's head is asked for to read the L0 frame in one request before its header is known.
const L0_READ_GUESS: usize = 64 * 1024;

/// Reads a global day index where it lies — a local file, or an S3 object read a range per call — without
/// loading it: [`Self::open`] reads the sync time and L0, and a lookup reads one L1 page, cached, and the
/// login's year blocks. Shared by reference (`Arc`) between concurrent callers; the file is assumed not to
/// change while it is read.
pub struct GlobalDayIndexReader<TKey, TReader> {
    file: SizedReader<TReader>,
    sync_time: DateTimeAsMicroseconds,
    l0: Vec<SplitL0Interval<TKey>>,
    l1_cache: PageCache<Vec<(TKey, LoginDays)>>,
}

impl<TKey: PrimaryKeyFixedSize, TReader: ArchiveFileReader> GlobalDayIndexReader<TKey, TReader> {
    /// Reads the sync time and the L0 frame chain after it, refusing as `Corrupted`: a file shorter than a sync
    /// time and an L0 header, an L0 header that fails its hash — bytes that are not a global day index — a
    /// broken chain, intervals out of order, and a file that ends before or inside the L1 page at its highest
    /// offset.
    ///
    /// Panics when an L1 page of this key cannot fit its `u32` size.
    pub async fn open(reader: TReader) -> Result<Self, ArchiveError> {
        assert!(TKey::LEN > 0, "PrimaryKeyFixedSize::LEN must be positive");
        assert!(
            u32::try_from(day_index_l1_page_len::<TKey>(COMPACT_L1_CAPACITY)).is_ok(),
            "an L1 page must fit its u32 size"
        );

        let file = SizedReader::open(reader).await?;

        if file.size() < MIN_DAY_INDEX_LEN {
            return Err(ArchiveError::Corrupted(format!(
                "a global day index of {} bytes; it holds at least its sync time and an L0 header",
                file.size()
            )));
        }

        let sync_time = sync_time_of(&file.read_exact(0, GLOBAL_DAY_INDEX_SYNC_TIME_LEN).await?);
        let frames =
            read_split_chain(&file, DAY_INDEX_L0_AT, SplitFrameKind::L0, L0_READ_GUESS).await?;
        let l0 = decode_split_l0_chain::<TKey>(&frames)?;

        let l1_offsets = || l0.iter().map(|interval| interval.l1_offset);
        refuse_l1_offsets_past_the_index(l1_offsets(), file.size())?;
        refuse_an_index_cut_inside_its_last_l1_page(&file, l1_offsets()).await?;

        Ok(Self {
            file,
            sync_time,
            l0,
            l1_cache: PageCache::new(DEFAULT_READER_L1_CACHE_PAGES),
        })
    }

    /// The sync time of the index `reader` reads, and nothing else: ONE `read_at` of its first 8 bytes — for
    /// an S3 object, the one ranged GET that says whether it is newer than a local copy.
    pub async fn read_sync_time(reader: &TReader) -> Result<DateTimeAsMicroseconds, ArchiveError> {
        let bytes = reader.read_at(0, GLOBAL_DAY_INDEX_SYNC_TIME_LEN).await?;
        Ok(sync_time_of(&bytes))
    }

    pub fn sync_time(&self) -> DateTimeAsMicroseconds {
        self.sync_time
    }

    /// In pages. A shrink evicts down to `pages` before it returns, and the cache stays within it.
    pub async fn set_l1_cache_size(&self, pages: usize) {
        self.l1_cache.resize(pages).await;
    }

    /// The sum of the `count` of every L1 frame header: one small read per L1 frame, uncached.
    pub async fn logins_count(&self) -> Result<usize, ArchiveError> {
        count_split_l1_keys(&self.file, &self.l0, check_day_index_l1_header::<TKey>).await
    }

    /// Every day `login` has data on from the day of `from` to the day of `to`, both included, ascending, each
    /// as its first microsecond. A login the index does not hold, and `from > to`, is an empty list; a failed
    /// read or a damaged page is `Err`, never a shorter list.
    pub async fn days_with_data(
        &self,
        login: &TKey,
        from: DateTimeAsMicroseconds,
        to: DateTimeAsMicroseconds,
    ) -> Result<Vec<DateTimeAsMicroseconds>, ArchiveError> {
        if from > to {
            return Ok(Vec::new());
        }

        let Some(days) =
            split_data_ref_of(&self.l0, login, |interval| self.l1_page(interval)).await?
        else {
            return Ok(Vec::new());
        };

        Ok(days_between(&self.year_blocks(days).await?, from, to))
    }

    /// The intervals of L0, in order.
    pub(crate) fn l0(&self) -> &[SplitL0Interval<TKey>] {
        &self.l0
    }

    /// The L1 page of `interval`, read through no cache.
    pub(crate) async fn source_l1_page(
        &self,
        interval: &SplitL0Interval<TKey>,
    ) -> Result<Vec<(TKey, LoginDays)>, ArchiveError> {
        let frames = read_split_chain(
            &self.file,
            interval.l1_offset as u64,
            SplitFrameKind::L1,
            day_index_l1_page_len::<TKey>(COMPACT_L1_CAPACITY),
        )
        .await?;

        decode_day_index_l1_chain(&frames, interval)
    }

    /// The year blocks `days` names, each checked.
    pub(crate) async fn year_blocks(&self, days: LoginDays) -> Result<Vec<YearDays>, ArchiveError> {
        let bytes = self
            .file
            .read_exact(days.offset as u64, days.blocks as usize * YEAR_BLOCK_LEN)
            .await?;

        decode_year_blocks(&bytes, days.offset as u64)
    }

    async fn l1_page(
        &self,
        interval: &SplitL0Interval<TKey>,
    ) -> Result<Arc<Vec<(TKey, LoginDays)>>, ArchiveError> {
        split_l1_page(&self.l1_cache, interval, self.source_l1_page(interval)).await
    }
}

/// The sync time in the first [`GLOBAL_DAY_INDEX_SYNC_TIME_LEN`] bytes of `bytes`.
pub(crate) fn sync_time_of(bytes: &[u8]) -> DateTimeAsMicroseconds {
    let value = bytes[..GLOBAL_DAY_INDEX_SYNC_TIME_LEN].try_into().unwrap();
    DateTimeAsMicroseconds::new(i64::from_le_bytes(value))
}
