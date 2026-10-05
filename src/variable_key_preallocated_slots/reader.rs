use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{
    ArchiveError, ArchiveFileReader, DEFAULT_L1_PAGE_BYTES, DEFAULT_MAX_DATA_FILE_BYTES,
    DEFAULT_READER_DATA_CACHE_PAGES, DEFAULT_READER_L1_CACHE_PAGES, DataRef, IoReaderFactory,
    LazyDataReaders, PageCache, PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyVariableSize,
    SizedReader, SplitFrame, SplitFrameKind, SplitL0Interval, SplitL1Page, VariableSplitL1Entry,
    check_max_data_file_bytes, check_split_data_page_sizes, check_variable_split_l1_frame_header,
    count_split_l1_keys, decode_variable_split_l0_chain, decode_variable_split_l1_chain,
    open_split_index, read_split_chain, read_split_data_body, split_data_body, split_data_page_len,
    split_data_ref_of, split_get_items, split_l1_page,
};

/// Reads a WORKING or a COMPACT split archive of variable-size keys through the readers of one factory —
/// local files, or S3 objects read a range per call — and is shared by reference (`Arc`) between
/// concurrent callers.
///
/// The index's L0 chain is read once by [`Self::open`] and lives in memory, with no lock, for the
/// reader's life. Data files are opened on first use, once each, and kept. L1 pages and data pages are
/// cached, each cache bounded by page count under its own `RwLock`: a hit stamps the page's `last_access`
/// under the read lock; a miss reads with no cache lock held, then write-locks only to insert — dropping
/// its own copy if another caller inserted the page meanwhile — and evicts the page with the oldest
/// `last_access` when full. `read_at` has no position, so misses do not wait on each other either.
///
/// Every read is exact. The size of each file is learned once, when it is opened, and every read is
/// clamped to it: the usual page is still one `read_at`, and a frame whose size reaches past its file is
/// `Corrupted` without a read past the end.
///
/// The archive is assumed immutable while it is read.
pub struct VariableKeyPreallocatedSlotsArchiveReader<
    TPrimaryKey,
    TSlotKey,
    TItem,
    TFactory,
    TReader,
> {
    pub(crate) factory: TFactory,
    pub(crate) index: SizedReader<TReader>,
    pub(crate) l0: Vec<SplitL0Interval<TPrimaryKey>>,
    l0_frames: usize,
    pub(crate) data_files: LazyDataReaders<TReader>,
    pub(crate) l1_cache: PageCache<SplitL1Page<TPrimaryKey>>,
    pub(crate) data_cache: PageCache<Vec<u8>>,
    max_data_file_bytes: AtomicU64,
    _types: PhantomData<fn() -> (TSlotKey, TItem)>,
}

impl<TPrimaryKey, TSlotKey, TItem, TFactory, TReader>
    VariableKeyPreallocatedSlotsArchiveReader<TPrimaryKey, TSlotKey, TItem, TFactory, TReader>
where
    TPrimaryKey: PrimaryKeyVariableSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TFactory: IoReaderFactory<TReader>,
    TReader: ArchiveFileReader,
{
    /// Opens the index and reads its L0 chain. A `next` that does not point past the end of its own frame
    /// or points past the end of the index, a frame whose entries disagree with its `count`, and intervals
    /// that are not ordered and disjoint across the whole chain refuse the archive as corrupted. So do an
    /// L0 header that fails its hash — bytes that are not a split index, such as a SingleFile object — and
    /// an index that ends before or inside an L1 page its L0 names, which is what a compaction that failed
    /// in its index pass leaves; the last L1 page's header costs one small read. No data file is opened.
    ///
    /// Panics when a data page of these types cannot fit its header fields (the same check the writer
    /// makes).
    pub async fn open(factory: TFactory) -> Result<Self, ArchiveError> {
        check_split_data_page_sizes::<TSlotKey, TItem>();

        let (index, l0, l0_frames) =
            open_split_index(&factory, decode_variable_split_l0_chain).await?;

        Ok(Self {
            factory,
            index,
            l0,
            l0_frames,
            data_files: LazyDataReaders::new(),
            l1_cache: PageCache::new(DEFAULT_READER_L1_CACHE_PAGES),
            data_cache: PageCache::new(DEFAULT_READER_DATA_CACHE_PAGES),
            max_data_file_bytes: AtomicU64::new(DEFAULT_MAX_DATA_FILE_BYTES),
            _types: PhantomData,
        })
    }

    /// In pages. A shrink evicts down to `pages` before it returns — 0 empties the cache and caches nothing
    /// more — and the cache stays within it.
    pub async fn set_l1_cache_size(&self, pages: usize) {
        self.l1_cache.resize(pages).await;
    }

    /// In pages. A shrink evicts down to `pages` before it returns — 0 empties the cache and caches nothing
    /// more — and the cache stays within it.
    pub async fn set_data_cache_size(&self, pages: usize) {
        self.data_cache.resize(pages).await;
    }

    /// The data file size the next [`Self::compaction_plan`] lays the compact data files out with;
    /// [`DEFAULT_MAX_DATA_FILE_BYTES`] until set. It says nothing about the archive being read.
    ///
    /// Panics above 4 GiB or below one data page.
    pub fn set_max_data_file_bytes(&self, bytes: u64) {
        check_max_data_file_bytes(bytes, split_data_page_len::<TSlotKey, TItem>());
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

    /// The items of slots `from..=to` of `key`, in slot order; a slot never written is
    /// `TItem::default()`.
    ///
    /// * `Ok(None)` — the archive does not hold the key;
    /// * `from > to` — `Ok(Some(vec![]))` at once, WITHOUT reading anything, so it says nothing about
    ///   whether the key is there;
    /// * `Err` — a read failed, the archive is corrupted, or a slot index is out of range. A failed read
    ///   is never an absent key.
    pub async fn get(
        &self,
        key: &TPrimaryKey,
        from: TSlotKey,
        to: TSlotKey,
    ) -> Result<Option<Vec<TItem>>, ArchiveError> {
        split_get_items(from, to, || self.data_ref_of(key), |at| self.data_body(at)).await
    }

    /// The sum of the `count` of every L1 frame header: one small read per L1 frame, uncached.
    pub async fn keys_count(&self) -> Result<usize, ArchiveError> {
        count_split_l1_keys(&self.index, &self.l0, check_variable_split_l1_frame_header).await
    }

    pub(crate) async fn data_ref_of(
        &self,
        key: &TPrimaryKey,
    ) -> Result<Option<DataRef>, ArchiveError> {
        split_data_ref_of(&self.l0, key, |interval| self.l1_page(interval)).await
    }

    /// The L1 page of `interval` read through no cache — the first frame in one read of a page of the
    /// default size (clamped to the index), then every continuation — with the frames its entries point
    /// into.
    pub(crate) async fn source_l1_page(
        &self,
        interval: &SplitL0Interval<TPrimaryKey>,
    ) -> Result<(Vec<SplitFrame>, Vec<VariableSplitL1Entry<TPrimaryKey>>), ArchiveError> {
        let frames = read_split_chain(
            &self.index,
            interval.l1_offset as u64,
            SplitFrameKind::L1,
            DEFAULT_L1_PAGE_BYTES,
        )
        .await?;

        let entries = decode_variable_split_l1_chain(&frames, interval)?;
        Ok((frames, entries))
    }

    /// The `SLOTS × ITEM_LEN` body of the data page at `at`, read through no cache.
    pub(crate) async fn source_data_body(&self, at: DataRef) -> Result<Vec<u8>, ArchiveError> {
        read_split_data_body::<TSlotKey, TItem, _, _>(&self.data_files, &self.factory, at).await
    }

    /// The L1 page of `interval` through the L1 cache, checked to span `interval` whether it was cached or
    /// just read.
    pub(crate) async fn l1_page(
        &self,
        interval: &SplitL0Interval<TPrimaryKey>,
    ) -> Result<Arc<SplitL1Page<TPrimaryKey>>, ArchiveError> {
        let load = async {
            let (_, entries) = self.source_l1_page(interval).await?;

            Ok(entries
                .into_iter()
                .map(|entry| (entry.key, entry.data))
                .collect())
        };

        split_l1_page(&self.l1_cache, interval, load).await
    }

    async fn data_body(&self, at: DataRef) -> Result<Arc<Vec<u8>>, ArchiveError> {
        split_data_body::<TSlotKey, TItem, _, _>(
            &self.data_cache,
            &self.data_files,
            &self.factory,
            at,
        )
        .await
    }
}
