use std::marker::PhantomData;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncSeek};
use tokio::sync::Mutex;

use crate::{
    ArchiveError, CheckedSlotIndex, DEFAULT_L1_CAPACITY, DEFAULT_READER_DATA_CACHE_PAGES,
    DEFAULT_READER_L1_CACHE_PAGES, L0Entry, L1_HEADER_LEN, PageCache, PreallocatedDataItem,
    PreallocatedSlotKey, PrimaryKeyFixedSize, check_type_sizes, decode_l0_chain, decode_l1_header,
    decode_l1_page, items_of_slots, l1_page_len, read_data_page, read_exact_at, read_l0_chain,
    read_self_sized,
};

/// Reads a WORKING or a COMPACT file out of any `AsyncRead + AsyncSeek` — a local file or an S3
/// object — and is shared by reference (`Arc`) between concurrent callers.
///
/// L0 is read once by [`Self::open`], following its chain of frames, and lives in memory, with no
/// lock, for the reader's life. L1 pages and data pages are cached by offset, each cache bounded by page
/// count under its own `RwLock`: a hit stamps the page's `last_access` under the read lock; a miss reads
/// the page from the source with no cache lock held (the source is one position, so its seek + read is
/// under a source mutex), then write-locks only to insert — dropping its own copy if another caller
/// inserted the page meanwhile — and evicts the page with the oldest `last_access` when full.
///
/// The file is assumed immutable while it is read.
///
/// **NOT CANCEL-SAFE over a source that keeps a read in flight.** Dropping a `get`, `keys_count` or
/// `write_compacted_to` future half way through a read leaves the shared source mid-request; an
/// `S3Reader` then refuses every later seek, so the NEXT caller fails. A caller that may be dropped
/// (a request handler) should run the call in a task of its own. `tokio::fs::File` is not affected.
pub struct FixedKeyPreallocatedSlotsArchiveReaderSingleFile<TPrimaryKey, TSlotKey, TItem, TSource> {
    pub(crate) source: Mutex<TSource>,
    pub(crate) l0: Vec<L0Entry<TPrimaryKey>>,
    l0_frames: usize,
    pub(crate) l1_cache: PageCache<Vec<(TPrimaryKey, u64)>>,
    pub(crate) data_cache: PageCache<Vec<u8>>,
    _types: PhantomData<fn() -> (TSlotKey, TItem)>,
}

impl<TPrimaryKey, TSlotKey, TItem, TSource>
    FixedKeyPreallocatedSlotsArchiveReaderSingleFile<TPrimaryKey, TSlotKey, TItem, TSource>
where
    TPrimaryKey: PrimaryKeyFixedSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TSource: AsyncRead + AsyncSeek + Unpin,
{
    /// Reads the L0 chain. A `next` that does not point past the end of its own frame (into frame 0,
    /// back into an earlier frame, a cycle) or past the end of the file, and intervals that are not
    /// ordered and disjoint across the whole chain, refuse the file as corrupted.
    ///
    /// Panics when a page of these types cannot fit its `u32` size (the same check the writer makes).
    pub async fn open(mut source: TSource) -> Result<Self, ArchiveError> {
        check_type_sizes::<TPrimaryKey, TSlotKey, TItem>(1, DEFAULT_L1_CAPACITY);

        let frames = read_l0_chain(&mut source).await?;
        let l0 = decode_l0_chain(&frames)?;

        Ok(Self {
            source: Mutex::new(source),
            l0,
            l0_frames: frames.len(),
            l1_cache: PageCache::new(DEFAULT_READER_L1_CACHE_PAGES),
            data_cache: PageCache::new(DEFAULT_READER_DATA_CACHE_PAGES),
            _types: PhantomData,
        })
    }

    /// In pages; 0 caches nothing. A shrink evicts at the next insert.
    pub fn set_l1_cache_size(&self, pages: usize) {
        self.l1_cache.set_capacity(pages);
    }

    /// In pages; 0 caches nothing. A shrink evicts at the next insert.
    pub fn set_data_cache_size(&self, pages: usize) {
        self.data_cache.set_capacity(pages);
    }

    pub fn l0_intervals(&self) -> usize {
        self.l0.len()
    }

    /// How many frames the L0 chain was read from; 1 for every compact file.
    pub fn l0_frames(&self) -> usize {
        self.l0_frames
    }

    /// The items of slots `from..=to` of `key`, in slot order; a slot never written is
    /// `TItem::default()`.
    ///
    /// * `Ok(None)` — the file does not hold the key;
    /// * `from > to` — `Ok(Some(vec![]))` at once, WITHOUT reading anything, so it says nothing about
    ///   whether the key is there;
    /// * `Err` — the source failed, the file is corrupted, or a slot index is out of range. A failed
    ///   read is never an absent key.
    pub async fn get(
        &self,
        key: &TPrimaryKey,
        from: TSlotKey,
        to: TSlotKey,
    ) -> Result<Option<Vec<TItem>>, ArchiveError> {
        let from = from.checked_slot_index()?;
        let to = to.checked_slot_index()?;

        if from > to {
            return Ok(Some(Vec::new()));
        }

        let Some(data_offset) = self.data_offset_of(key).await? else {
            return Ok(None);
        };

        let page = self.data_page(data_offset).await?;
        Ok(Some(items_of_slots::<TItem>(&page, from, to)))
    }

    /// The sum of the `count` of every L1 page header: one small read per L1 page, uncached.
    pub async fn keys_count(&self) -> Result<usize, ArchiveError> {
        let mut total = 0;

        for interval in &self.l0 {
            let mut header = [0u8; L1_HEADER_LEN];
            read_exact_at(&mut *self.source.lock().await, interval.offset, &mut header).await?;
            let (_, count) = decode_l1_header::<TPrimaryKey>(&header, interval.offset)?;
            total += count;
        }

        Ok(total)
    }

    pub(crate) async fn data_offset_of(
        &self,
        key: &TPrimaryKey,
    ) -> Result<Option<u64>, ArchiveError> {
        let index = self.l0.partition_point(|entry| entry.to < *key);

        let Some(interval) = self.l0.get(index).filter(|entry| entry.from <= *key) else {
            return Ok(None);
        };

        let page = self.l1_page(interval).await?;

        Ok(page
            .binary_search_by(|(stored, _)| stored.cmp(key))
            .ok()
            .map(|at| page[at].1))
    }

    async fn l1_page(
        &self,
        interval: &L0Entry<TPrimaryKey>,
    ) -> Result<Arc<Vec<(TPrimaryKey, u64)>>, ArchiveError> {
        if let Some(page) = self.l1_cache.get(interval.offset).await {
            return Ok(page);
        }

        let bytes = self.read_l1_bytes(interval.offset).await?;
        let entries = decode_l1_page(&bytes, interval)?;

        Ok(self.l1_cache.insert(interval.offset, entries).await)
    }

    async fn data_page(&self, offset: u64) -> Result<Arc<Vec<u8>>, ArchiveError> {
        if let Some(page) = self.data_cache.get(offset).await {
            return Ok(page);
        }

        let bytes = self.read_data_bytes(offset).await?;
        Ok(self.data_cache.insert(offset, bytes).await)
    }

    /// A whole L1 page: first as many bytes as a page of the default capacity, then the rest if its
    /// header says it is longer.
    pub(crate) async fn read_l1_bytes(&self, offset: u64) -> Result<Vec<u8>, ArchiveError> {
        let guess = l1_page_len::<TPrimaryKey>(DEFAULT_L1_CAPACITY);
        let mut source = self.source.lock().await;

        read_self_sized(&mut *source, offset, guess, L1_HEADER_LEN, |header| {
            decode_l1_header::<TPrimaryKey>(header, offset).map(|(size, _)| size)
        })
        .await
    }

    pub(crate) async fn read_data_bytes(&self, offset: u64) -> Result<Vec<u8>, ArchiveError> {
        let mut source = self.source.lock().await;
        read_data_page::<TSlotKey, TItem, _>(&mut *source, offset).await
    }
}
