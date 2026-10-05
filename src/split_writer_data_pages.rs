use std::marker::PhantomData;

use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, DEFAULT_WRITER_DATA_CACHE_PAGES, DataRef,
    IoWriterFactory, PreallocatedDataItem, PreallocatedSlotKey, SplitDataWriters, SplitIndexWriter,
    WriterPageCache, empty_split_data_page, split_data_page_len, split_slot_at,
};

/// A split writer's data pages: preallocated with `TItem::default()`, cached up to `cache_size`,
/// reaching their data file on eviction or on [`Self::flush`].
///
/// A data file grows only by append, so a page's bytes may reach it only after every page before it in
/// the same file. The invariant that makes this cheap: **a page past its file's append frontier is always
/// cached.** A new page is cached (or appended at once with no cache); a cached page leaves the cache only
/// through [`Self::store`], which appends every page before it that is not appended yet — all of them
/// cached, by the same invariant. So an item for a page that is not cached goes straight to its file.
///
/// Every call that may write takes the archive's `index`, whose frame 0 hash the data files spoil before
/// their first byte after a completed flush (see [`SplitDataWriters`]).
pub(crate) struct SplitWriterDataPages<TSlotKey, TItem> {
    cache_size: usize,
    pages: WriterPageCache<Vec<u8>>,
    empty: Option<Vec<u8>>,
    _types: PhantomData<fn() -> (TSlotKey, TItem)>,
}

impl<TSlotKey: PreallocatedSlotKey, TItem: PreallocatedDataItem>
    SplitWriterDataPages<TSlotKey, TItem>
{
    pub fn new() -> Self {
        Self {
            cache_size: DEFAULT_WRITER_DATA_CACHE_PAGES,
            pages: WriterPageCache::new(),
            empty: None,
            _types: PhantomData,
        }
    }

    /// 0 appends every new page and writes every item straight to its file.
    pub fn set_cache_size(&mut self, pages: usize) {
        self.cache_size = pages;
    }

    pub fn page_len(&self) -> usize {
        split_data_page_len::<TSlotKey, TItem>()
    }

    /// A new page for the key of `rank`. A rank needing data file 256 is refused before anything changes.
    pub async fn preallocate<TWriter, TReader, TFactory>(
        &mut self,
        files: &mut SplitDataWriters<TWriter>,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        factory: &TFactory,
        rank: usize,
    ) -> Result<DataRef, ArchiveError>
    where
        TWriter: ArchiveFileWriter,
        TReader: ArchiveFileReader,
        TFactory: IoWriterFactory<TWriter>,
    {
        let at = files.reserve_page(factory, rank).await?;

        if self.empty.is_none() {
            self.empty = Some(empty_split_data_page::<TSlotKey, TItem>()?);
        }

        let page = self.empty.clone().unwrap();

        if self.cache_size == 0 {
            self.evict(files, index, 0).await?;
            files.append_page(index, at, &page).await?;
            return Ok(at);
        }

        self.evict(files, index, self.cache_size - 1).await?;
        self.pages.insert(at.cache_key(), page, true);

        Ok(at)
    }

    pub async fn write_item<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
        &mut self,
        files: &SplitDataWriters<TWriter>,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        at: DataRef,
        slot: usize,
        item: &[u8],
    ) -> Result<(), ArchiveError> {
        let within = split_slot_at::<TItem>(slot);

        if let Some(page) = self.pages.modify(at.cache_key()) {
            page[within..within + item.len()].copy_from_slice(item);
            return Ok(());
        }

        files.write_at(index, at, within, item).await
    }

    /// Every dirty page, in file order.
    pub async fn flush<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
        &mut self,
        files: &mut SplitDataWriters<TWriter>,
        index: &mut SplitIndexWriter<TWriter, TReader>,
    ) -> Result<(), ArchiveError> {
        for key in self.pages.dirty_offsets() {
            self.store(files, index, key).await?;
        }

        Ok(())
    }

    async fn evict<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
        &mut self,
        files: &mut SplitDataWriters<TWriter>,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        keep: usize,
    ) -> Result<(), ArchiveError> {
        while self.pages.len() > keep {
            let Some(oldest) = self.pages.least_recently_used() else {
                break;
            };

            self.store(files, index, oldest).await?;
            self.pages.remove(oldest);
        }

        Ok(())
    }

    async fn store<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
        &mut self,
        files: &mut SplitDataWriters<TWriter>,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        key: u64,
    ) -> Result<(), ArchiveError> {
        let Some(page) = self.pages.get(key) else {
            return Ok(());
        };

        if !self.pages.is_dirty(key) {
            return Ok(());
        }

        let at = DataRef::from_cache_key(key);

        if files.is_appended(at) {
            files.write_at(index, at, 0, page).await?;
            self.pages.mark_clean(key);
            return Ok(());
        }

        let page_len = self.page_len() as u64;
        let mut offset = files.appended(at.file_no);

        while offset <= at.offset as u64 {
            let run = DataRef {
                file_no: at.file_no,
                offset: offset as u32,
            };

            let page = self
                .pages
                .get(run.cache_key())
                .expect("a data page past the append frontier is always cached");

            files.append_page(index, run, page).await?;
            self.pages.mark_clean(run.cache_key());
            offset += page_len;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
    use crate::{
        ArchiveError, DataFileLayout, SplitDataWriters, SplitIndexWriter, SplitWriterDataPages,
        TinyItem, TinySlot, data_frame,
    };

    type Pages = SplitWriterDataPages<TinySlot, TinyItem>;
    type Files = SplitDataWriters<InMemoryWriter>;
    type Index = SplitIndexWriter<InMemoryWriter, InMemoryReader>;

    /// A page with `items` in their slots and the empty item everywhere else.
    fn page(items: &[(usize, u8)]) -> Vec<u8> {
        let mut body = vec![0xEE; 12];
        for (slot, value) in items {
            body[slot * 4..slot * 4 + 4].copy_from_slice(&[*value; 4]);
        }
        data_frame(22, 0, 4, &body)
    }

    #[tokio::test]
    async fn evicting_a_page_appends_every_page_before_it_first() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &[0; 20]).await.unwrap();
        let mut files = Files::new(DataFileLayout::new(22, 88));
        let mut pages = Pages::new();
        pages.set_cache_size(4);
        assert_eq!(pages.page_len(), 22);

        let mut refs = Vec::new();
        for rank in 0..4 {
            refs.push(
                pages
                    .preallocate(&mut files, &mut index, &io, rank)
                    .await
                    .unwrap(),
            );
        }
        pages
            .write_item(&files, &mut index, refs[0], 1, &[1; 4])
            .await
            .unwrap();
        assert_eq!(files.appended(0), 0);

        // The least recently used page is page 1, and page 0 has to reach the file before it.
        refs.push(
            pages
                .preallocate(&mut files, &mut index, &io, 4)
                .await
                .unwrap(),
        );
        let data_0 = io.file(FileRole::Data(0)).unwrap();
        assert_eq!(files.appended(0), 44);
        assert_eq!(data_0.bytes().await, [page(&[(1, 1)]), page(&[])].concat());

        // Page 1 left the cache, so its item goes to the file; page 0 is still cached.
        pages
            .write_item(&files, &mut index, refs[1], 2, &[2; 4])
            .await
            .unwrap();
        pages
            .write_item(&files, &mut index, refs[0], 0, &[3; 4])
            .await
            .unwrap();
        assert_eq!(data_0.write_ats(), 1);

        pages.flush(&mut files, &mut index).await.unwrap();

        assert_eq!(
            data_0.bytes().await,
            [
                page(&[(0, 3), (1, 1)]),
                page(&[(2, 2)]),
                page(&[]),
                page(&[])
            ]
            .concat()
        );
        assert_eq!((data_0.appends(), data_0.write_ats()), (4, 2));

        let data_1 = io.file(FileRole::Data(1)).unwrap();
        assert_eq!(data_1.bytes().await, page(&[]));
        assert_eq!(
            io.created_roles(),
            [FileRole::Index, FileRole::Data(0), FileRole::Data(1)]
        );

        pages.flush(&mut files, &mut index).await.unwrap();
        assert_eq!((data_0.appends(), data_0.write_ats()), (4, 2));
    }

    #[tokio::test]
    async fn with_no_cache_every_page_is_appended_at_once_and_every_item_written_in_place() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &[0; 20]).await.unwrap();
        let mut files = Files::new(DataFileLayout::new(22, 44));
        let mut pages = Pages::new();
        pages.set_cache_size(0);

        let mut refs = Vec::new();
        for rank in 0..3 {
            refs.push(
                pages
                    .preallocate(&mut files, &mut index, &io, rank)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!((files.appended(0), files.appended(1)), (44, 22));

        pages
            .write_item(&files, &mut index, refs[2], 2, &[5; 4])
            .await
            .unwrap();
        pages
            .write_item(&files, &mut index, refs[0], 0, &[6; 4])
            .await
            .unwrap();
        pages.flush(&mut files, &mut index).await.unwrap();

        let data_0 = io.file(FileRole::Data(0)).unwrap();
        let data_1 = io.file(FileRole::Data(1)).unwrap();
        assert_eq!(data_0.bytes().await, [page(&[(0, 6)]), page(&[])].concat());
        assert_eq!(data_1.bytes().await, page(&[(2, 5)]));
        assert_eq!((data_0.appends(), data_0.write_ats()), (2, 1));
        assert_eq!((data_1.appends(), data_1.write_ats()), (1, 1));
    }

    #[tokio::test]
    async fn a_refused_257th_data_file_leaves_the_pages_as_they_were() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &[0; 20]).await.unwrap();
        let mut files = Files::new(DataFileLayout::new(22, 22));
        let mut pages = Pages::new();
        pages.set_cache_size(2);

        for rank in 0..256 {
            pages
                .preallocate(&mut files, &mut index, &io, rank)
                .await
                .unwrap();
        }

        assert!(matches!(
            pages.preallocate(&mut files, &mut index, &io, 256).await,
            Err(ArchiveError::ContractViolation(_))
        ));

        pages.flush(&mut files, &mut index).await.unwrap();
        assert_eq!(files.files(), 256);
        assert_eq!(
            io.file(FileRole::Data(255)).unwrap().bytes().await,
            page(&[])
        );
    }
}
