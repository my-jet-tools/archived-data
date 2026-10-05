use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, DEFAULT_WRITER_L1_CACHE_PAGES,
    DEFAULT_WRITER_L2_CACHE_FRAMES, MinuteCells, PrimaryKeyFixedSize, SplitIndexWriter,
    WriterPageCache, encode_records_l1_page,
};

/// The pages a records writer holds of its INDEX: L1 pages — every key with the offset of its L2 frame —
/// and L2 frames, each in a cache of its own size, reaching the index on eviction or on [`Self::flush`].
///
/// Both kinds share one index that grows only by append, in the order their offsets were reserved, so the
/// invariant spans both caches: **an index page past the append frontier is always cached**, as an L1 page
/// or as an L2 frame. A page leaving a cache past the frontier appends every page between the frontier and
/// it first, taking each from whichever cache holds it, so the index has no hole.
pub(crate) struct RecordsWriterIndexPages<TKey> {
    capacity_l1: usize,
    l1_cache_size: usize,
    l2_cache_size: usize,
    l1: WriterPageCache<Vec<(TKey, u32)>>,
    l2: WriterPageCache<MinuteCells>,
}

impl<TKey: PrimaryKeyFixedSize> RecordsWriterIndexPages<TKey> {
    pub fn new(capacity_l1: usize) -> Self {
        Self {
            capacity_l1,
            l1_cache_size: DEFAULT_WRITER_L1_CACHE_PAGES,
            l2_cache_size: DEFAULT_WRITER_L2_CACHE_FRAMES,
            l1: WriterPageCache::new(),
            l2: WriterPageCache::new(),
        }
    }

    pub fn set_l1_cache_size(&mut self, pages: usize) {
        self.l1_cache_size = pages.max(1);
    }

    pub fn set_l2_cache_size(&mut self, frames: usize) {
        self.l2_cache_size = frames;
    }

    /// The entries of a cached L1 page, NOT stamped as used.
    pub fn l1(&self, offset: u32) -> Option<&Vec<(TKey, u32)>> {
        self.l1.get(offset as u64)
    }

    pub fn touch_l1(&mut self, offset: u32) -> bool {
        self.l1.touch(offset as u64)
    }

    pub fn modify_l1(&mut self, offset: u32) -> Option<&mut Vec<(TKey, u32)>> {
        self.l1.modify(offset as u64)
    }

    /// Caches an L1 page as the most recently used, evicting nothing.
    pub fn insert_l1(&mut self, offset: u32, entries: Vec<(TKey, u32)>, dirty: bool) {
        self.l1.insert(offset as u64, entries, dirty);
    }

    /// A cached L2 frame, NOT stamped as used.
    pub fn l2(&self, offset: u32) -> Option<&MinuteCells> {
        self.l2.get(offset as u64)
    }

    pub fn touch_l2(&mut self, offset: u32) -> bool {
        self.l2.touch(offset as u64)
    }

    pub fn modify_l2(&mut self, offset: u32) -> Option<&mut MinuteCells> {
        self.l2.modify(offset as u64)
    }

    /// Caches an L2 frame as the most recently used, evicting nothing.
    pub fn insert_l2(&mut self, offset: u32, cells: MinuteCells, dirty: bool) {
        self.l2.insert(offset as u64, cells, dirty);
    }

    /// Stores and drops the least recently used pages of each cache until it is within its size.
    pub async fn evict<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
    ) -> Result<(), ArchiveError> {
        while self.l1.len() > self.l1_cache_size {
            let Some(oldest) = self.l1.least_recently_used() else {
                break;
            };

            self.store(index, oldest).await?;
            self.l1.remove(oldest);
        }

        while self.l2.len() > self.l2_cache_size {
            let Some(oldest) = self.l2.least_recently_used() else {
                break;
            };

            self.store(index, oldest).await?;
            self.l2.remove(oldest);
        }

        Ok(())
    }

    /// Every dirty page of both caches, in offset order.
    pub async fn flush<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
    ) -> Result<(), ArchiveError> {
        let mut offsets = self.l1.dirty_offsets();
        offsets.extend(self.l2.dirty_offsets());
        offsets.sort_unstable();

        for offset in offsets {
            self.store(index, offset).await?;
        }

        Ok(())
    }

    /// An appended page is rewritten in place. A page past the frontier is appended after every page between
    /// the frontier and it, each advancing the walk by its own length. A page that is not cached, or not
    /// dirty, is left alone.
    async fn store<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        offset: u64,
    ) -> Result<(), ArchiveError> {
        if !self.l1.is_dirty(offset) && !self.l2.is_dirty(offset) {
            return Ok(());
        }

        if index.is_appended(offset as u32) {
            let bytes = self.encode(offset)?.expect("a dirty page is cached");
            index.write_at(offset as u32, &bytes).await?;
            self.mark_clean(offset);
            return Ok(());
        }

        let mut run = index.appended();

        while run <= offset {
            let Some(bytes) = self.encode(run)? else {
                return Err(ArchiveError::ContractViolation(format!(
                    "the index page at {run} is past the end of the index and not cached: \
                     the writer is not usable after an earlier error"
                )));
            };

            index.append(run as u32, &bytes).await?;
            self.mark_clean(run);
            run += bytes.len() as u64;
        }

        Ok(())
    }

    fn encode(&self, offset: u64) -> Result<Option<Vec<u8>>, ArchiveError> {
        if let Some(entries) = self.l1.get(offset) {
            return encode_records_l1_page(entries, self.capacity_l1).map(Some);
        }

        Ok(self.l2.get(offset).map(MinuteCells::encode))
    }

    fn mark_clean(&mut self, offset: u64) {
        self.l1.mark_clean(offset);
        self.l2.mark_clean(offset);
    }
}
