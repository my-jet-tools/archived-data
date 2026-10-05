use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, DEFAULT_WRITER_L1_CACHE_PAGES,
    SplitIndexWriter, WriterPageCache,
};

/// A split writer's L1 pages in memory, each a list of the module's entries, cached up to `cache_size` and
/// reaching the index on eviction or on [`Self::flush`]. Every page is `page_len` bytes; what its bytes are
/// is the module's, handed in as `encode` wherever a page is written.
///
/// The index grows only by append, so the invariant is the data pages' one: **a page past the index's
/// append frontier is always cached.** A page leaving the cache goes through [`Self::store`], which appends
/// every page between the frontier and it first — all cached, by the same invariant — so the index has no
/// hole.
pub(crate) struct SplitWriterL1Pages<TEntry> {
    cache_size: usize,
    page_len: usize,
    pages: WriterPageCache<Vec<TEntry>>,
}

impl<TEntry> SplitWriterL1Pages<TEntry> {
    pub fn new(page_len: usize) -> Self {
        Self {
            cache_size: DEFAULT_WRITER_L1_CACHE_PAGES,
            page_len,
            pages: WriterPageCache::new(),
        }
    }

    /// At least 1. A smaller cache only costs IO.
    pub fn set_cache_size(&mut self, pages: usize) {
        self.cache_size = pages.max(1);
    }

    pub fn cache_size(&self) -> usize {
        self.cache_size
    }

    pub fn page_len(&self) -> usize {
        self.page_len
    }

    /// The entries of a cached page, NOT stamped as used.
    pub fn get(&self, offset: u64) -> Option<&Vec<TEntry>> {
        self.pages.get(offset)
    }

    /// Stamps a cached page as the most recently used; `false` when it is not cached.
    pub fn touch(&mut self, offset: u64) -> bool {
        self.pages.touch(offset)
    }

    /// The entries of a cached page to change: stamped as used and marked dirty.
    pub fn modify(&mut self, offset: u64) -> Option<&mut Vec<TEntry>> {
        self.pages.modify(offset)
    }

    /// Caches a page as the most recently used, and evicts NOTHING: the caller evicts once no page it still
    /// needs in memory can be the victim. A new page is `dirty`; a page read back from the index is not.
    pub fn insert(&mut self, offset: u64, entries: Vec<TEntry>, dirty: bool) {
        self.pages.insert(offset, entries, dirty);
    }

    /// Stores and drops least recently used pages until `keep` are left.
    pub async fn evict<TWriter, TReader, TEncode>(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        keep: usize,
        encode: &TEncode,
    ) -> Result<(), ArchiveError>
    where
        TWriter: ArchiveFileWriter,
        TReader: ArchiveFileReader,
        TEncode: Fn(&[TEntry]) -> Result<Vec<u8>, ArchiveError>,
    {
        while self.pages.len() > keep {
            let Some(oldest) = self.pages.least_recently_used() else {
                break;
            };

            self.store(index, oldest, encode).await?;
            self.pages.remove(oldest);
        }

        Ok(())
    }

    /// Every dirty page, in offset order.
    pub async fn flush<TWriter, TReader, TEncode>(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        encode: &TEncode,
    ) -> Result<(), ArchiveError>
    where
        TWriter: ArchiveFileWriter,
        TReader: ArchiveFileReader,
        TEncode: Fn(&[TEntry]) -> Result<Vec<u8>, ArchiveError>,
    {
        for offset in self.pages.dirty_offsets() {
            self.store(index, offset, encode).await?;
        }

        Ok(())
    }

    /// An appended page is rewritten in place. A page past the frontier is appended after every page
    /// between the frontier and it, so the index has no hole. A page that is not cached, or not dirty, is
    /// left alone.
    pub async fn store<TWriter, TReader, TEncode>(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        offset: u64,
        encode: &TEncode,
    ) -> Result<(), ArchiveError>
    where
        TWriter: ArchiveFileWriter,
        TReader: ArchiveFileReader,
        TEncode: Fn(&[TEntry]) -> Result<Vec<u8>, ArchiveError>,
    {
        let Some(page) = self.pages.get(offset) else {
            return Ok(());
        };

        if !self.pages.is_dirty(offset) {
            return Ok(());
        }

        if index.is_appended(offset as u32) {
            let bytes = encode(page)?;
            index.write_at(offset as u32, &bytes).await?;
            self.pages.mark_clean(offset);
            return Ok(());
        }

        let mut run = index.appended();

        while run <= offset {
            let Some(page) = self.pages.get(run) else {
                return Err(ArchiveError::ContractViolation(format!(
                    "the L1 page at {run} is past the end of the index and not cached: \
                     the writer is not usable after an earlier error"
                )));
            };

            let bytes = encode(page)?;
            index.append(run as u32, &bytes).await?;
            self.pages.mark_clean(run);
            run += self.page_len as u64;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
    use crate::{ArchiveError, SplitIndexWriter, SplitWriterL1Pages};

    type Index = SplitIndexWriter<InMemoryWriter, InMemoryReader>;

    /// A page of 4 bytes: its entries, then zeros.
    fn encode(entries: &[u8]) -> Result<Vec<u8>, ArchiveError> {
        let mut bytes = entries.to_vec();
        bytes.resize(4, 0);
        Ok(bytes)
    }

    async fn index(io: &InMemoryIo) -> Index {
        Index::create(io, &[0xAA; 2]).await.unwrap()
    }

    #[tokio::test]
    async fn evicting_a_page_past_the_frontier_appends_every_page_before_it() {
        let io = InMemoryIo::default();
        let mut index = index(&io).await;
        let mut pages = SplitWriterL1Pages::new(4);
        pages.set_cache_size(0);
        assert_eq!((pages.cache_size(), pages.page_len()), (1, 4));

        for entry in 1..=3u8 {
            let offset = index.reserve(4).unwrap() as u64;
            pages.insert(offset, vec![entry], true);
        }

        // The page at 6 is used longest ago, and the page at 2 before it has to reach the index first.
        pages.touch(2);
        pages.touch(10);
        pages.modify(2).unwrap().push(9);
        pages.evict(&mut index, 2, &encode).await.unwrap();

        let file = io.file(FileRole::Index).unwrap();
        assert_eq!(
            file.bytes().await,
            [&[0xAA, 0xAA][..], &[1, 9, 0, 0], &[2, 0, 0, 0]].concat()
        );
        assert_eq!(pages.get(2), Some(&vec![1, 9]));
        assert!(pages.get(6).is_none() && pages.get(10).is_some());
        assert_eq!(file.appends(), 3);
    }

    #[tokio::test]
    async fn an_appended_page_is_rewritten_in_place_and_a_clean_one_not_at_all() {
        let io = InMemoryIo::default();
        let mut index = index(&io).await;
        let mut pages = SplitWriterL1Pages::new(4);

        let first = index.reserve(4).unwrap() as u64;
        let second = index.reserve(4).unwrap() as u64;
        pages.insert(first, vec![1], true);
        pages.insert(second, vec![2], true);
        pages.flush(&mut index, &encode).await.unwrap();

        let file = io.file(FileRole::Index).unwrap();
        assert_eq!((file.appends(), file.write_ats()), (3, 0));

        pages.flush(&mut index, &encode).await.unwrap();
        assert_eq!((file.appends(), file.write_ats()), (3, 0));

        pages.modify(first).unwrap().push(7);
        pages.flush(&mut index, &encode).await.unwrap();
        assert_eq!((file.appends(), file.write_ats()), (3, 1));
        assert_eq!(file.bytes().await[2..6], [1, 7, 0, 0]);

        pages.insert(second, vec![5], false);
        pages.store(&mut index, second, &encode).await.unwrap();
        pages.store(&mut index, 99, &encode).await.unwrap();
        assert_eq!((file.appends(), file.write_ats()), (3, 1));
    }

    #[tokio::test]
    async fn a_page_past_the_frontier_that_is_not_cached_is_a_contract_violation() {
        let io = InMemoryIo::default();
        let mut index = index(&io).await;
        let mut pages = SplitWriterL1Pages::new(4);

        index.reserve(4).unwrap();
        let second = index.reserve(4).unwrap() as u64;
        pages.insert(second, vec![2], true);

        assert!(matches!(
            pages.store(&mut index, second, &encode).await,
            Err(ArchiveError::ContractViolation(_))
        ));
    }
}
