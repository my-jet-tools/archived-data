use crate::{
    AppendFrontier, ArchiveError, ArchiveFileReader, ArchiveFileWriter, DataFileLayout, DataRef,
    IoWriterFactory, SplitIndexWriter,
};

/// The data files of an archive being written. Pages are reserved in rank order, which the layout turns
/// into a data file and an offset; a data file is created when its first page is reserved, and every one
/// created stays open until the end, because a known key's page may live in any earlier file.
///
/// Every byte reaches a data file through [`Self::append_page`] or [`Self::write_at`], and both spoil frame
/// 0's hash in the archive's `index` first (see [`SplitIndexWriter::spoil_frame_0`]): a data page changed
/// after a completed flush is a change the index it completed does not describe.
pub(crate) struct SplitDataWriters<TWriter> {
    writers: Vec<TWriter>,
    frontiers: Vec<AppendFrontier>,
    layout: DataFileLayout,
}

impl<TWriter: ArchiveFileWriter> SplitDataWriters<TWriter> {
    pub fn new(layout: DataFileLayout) -> Self {
        Self {
            writers: Vec::new(),
            frontiers: Vec::new(),
            layout,
        }
    }

    /// Panics once a data file exists: every page already placed was placed by the old layout.
    pub fn set_layout(&mut self, layout: DataFileLayout) {
        assert!(
            self.writers.is_empty(),
            "the data file size cannot change once a data page is placed"
        );
        self.layout = layout;
    }

    pub fn layout(&self) -> DataFileLayout {
        self.layout
    }

    pub fn files(&self) -> usize {
        self.writers.len()
    }

    /// The place of the page of `rank`, the next rank after the last one reserved. A rank needing data
    /// file 256 is refused before anything is created or reserved.
    pub async fn reserve_page<TFactory: IoWriterFactory<TWriter>>(
        &mut self,
        factory: &TFactory,
        rank: usize,
    ) -> Result<DataRef, ArchiveError> {
        let at = self.layout.ref_of(rank)?;
        let no = at.file_no as usize;

        let full_file = self.layout.pages_per_file() * self.layout.page_len() as u64;

        let expected = match self.frontiers.get(no) {
            Some(frontier) if no + 1 == self.frontiers.len() => Some(frontier.reserved()),
            None if no == self.frontiers.len()
                && self
                    .frontiers
                    .last()
                    .is_none_or(|last| last.reserved() == full_file) =>
            {
                Some(0)
            }
            _ => None,
        };

        if expected != Some(at.offset as u64) {
            return Err(ArchiveError::ContractViolation(format!(
                "data page {rank} belongs at {at:?}, which is not the next page of {} data files",
                self.frontiers.len()
            )));
        }

        if no == self.writers.len() {
            let writer = factory.create_data_writer(at.file_no).await?;
            self.writers.push(writer);
            self.frontiers.push(AppendFrontier::new(0));
        }

        self.frontiers[no].reserve(self.layout.page_len());
        Ok(at)
    }

    /// Pages are appended whole, so a page is appended as soon as its first byte is.
    pub fn is_appended(&self, at: DataRef) -> bool {
        self.frontiers
            .get(at.file_no as usize)
            .is_some_and(|frontier| frontier.is_appended(at.offset as u64))
    }

    /// Panics on a data file that was never created.
    pub fn appended(&self, file_no: u8) -> u64 {
        self.frontiers[file_no as usize].appended()
    }

    /// Frame 0's hash in `index` is spoiled first, if it verifies.
    pub async fn append_page<TReader: ArchiveFileReader>(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        at: DataRef,
        page: &[u8],
    ) -> Result<(), ArchiveError> {
        let no = self.created(at)?;

        if page.len() != self.layout.page_len() {
            return Err(ArchiveError::ContractViolation(format!(
                "a data page of {} bytes; the layout's pages are {}",
                page.len(),
                self.layout.page_len()
            )));
        }

        self.frontiers[no].check_append(at.offset as u64, page.len())?;
        index.spoil_frame_0().await?;
        let returned = self.writers[no].append(page).await?;
        self.frontiers[no].confirm_append(returned, page.len())
    }

    /// Overwrites `bytes` at `within` of an appended page. Frame 0's hash in `index` is spoiled first, if it
    /// verifies.
    pub async fn write_at<TReader: ArchiveFileReader>(
        &self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        at: DataRef,
        within: usize,
        bytes: &[u8],
    ) -> Result<(), ArchiveError> {
        let no = self.created(at)?;

        if within + bytes.len() > self.layout.page_len() || !self.is_appended(at) {
            return Err(ArchiveError::ContractViolation(format!(
                "{} bytes at {within} of the data page at {at:?}, which is not an appended page of {} bytes",
                bytes.len(),
                self.layout.page_len()
            )));
        }

        index.spoil_frame_0().await?;

        Ok(self.writers[no]
            .write_at(at.offset as u64 + within as u64, bytes)
            .await?)
    }

    /// Every data file created, in file order.
    pub fn into_writers(self) -> Vec<TWriter> {
        self.writers
    }

    fn created(&self, at: DataRef) -> Result<usize, ArchiveError> {
        let no = at.file_no as usize;

        if no >= self.writers.len() {
            return Err(ArchiveError::ContractViolation(format!(
                "data file {no} is not created; there are {}",
                self.writers.len()
            )));
        }

        Ok(no)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplitL0FrameWrite;
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
    use crate::split_test_io::wait_until;

    type Files = SplitDataWriters<InMemoryWriter>;
    type Index = SplitIndexWriter<InMemoryWriter, InMemoryReader>;

    /// An index of 20 bytes whose frame 0 verifies, as a completed flush leaves it: the 8 bytes at 12 spoil
    /// it.
    async fn verifying_index(io: &InMemoryIo) -> Index {
        let mut index = Index::create(io, &[0; 20]).await.unwrap();
        index.frame_0_verifies(SplitL0FrameWrite {
            offset: 12,
            append: false,
            bytes: vec![0xEE; 8],
        });
        index
    }

    fn at(file_no: u8, offset: u32) -> DataRef {
        DataRef { file_no, offset }
    }

    fn is_violation<T>(result: &Result<T, ArchiveError>) -> bool {
        matches!(result, Err(ArchiveError::ContractViolation(_)))
    }

    #[tokio::test]
    async fn a_data_file_is_created_by_its_first_page_and_pages_come_in_rank_order() {
        let io = InMemoryIo::default();
        let mut files = Files::new(DataFileLayout::new(100, 300));
        files.set_layout(DataFileLayout::new(100, 200));
        assert_eq!(files.layout().pages_per_file(), 2);

        assert_eq!(files.reserve_page(&io, 0).await.unwrap(), at(0, 0));
        assert_eq!(io.created_roles(), [FileRole::Data(0)]);
        assert_eq!(files.reserve_page(&io, 1).await.unwrap(), at(0, 100));
        assert_eq!(io.created_roles(), [FileRole::Data(0)]);
        assert_eq!(files.reserve_page(&io, 2).await.unwrap(), at(1, 0));
        assert_eq!(io.created_roles(), [FileRole::Data(0), FileRole::Data(1)]);
        assert_eq!(files.files(), 2);

        for rank in [0, 1, 2, 4, 5] {
            assert!(
                is_violation(&files.reserve_page(&io, rank).await),
                "rank {rank}"
            );
        }
        assert_eq!(files.files(), 2);
        assert_eq!(files.reserve_page(&io, 3).await.unwrap(), at(1, 100));
    }

    #[tokio::test]
    async fn pages_are_appended_at_the_frontier_and_patched_in_any_file() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &[0; 20]).await.unwrap();
        let mut files = Files::new(DataFileLayout::new(100, 200));

        let mut refs = Vec::new();
        for rank in 0..4 {
            refs.push(files.reserve_page(&io, rank).await.unwrap());
        }

        for (at, len) in [(refs[1], 100), (refs[0], 99), (at(2, 0), 100)] {
            let refused = files.append_page(&mut index, at, &vec![1; len]).await;
            assert!(is_violation(&refused), "{len} bytes at {at:?}");
        }

        for (rank, data_ref) in refs.iter().take(3).enumerate() {
            files
                .append_page(&mut index, *data_ref, &[rank as u8; 100])
                .await
                .unwrap();
        }

        assert!(files.is_appended(refs[2]));
        assert!(!files.is_appended(refs[3]));
        assert!(!files.is_appended(at(9, 0)));
        assert_eq!((files.appended(0), files.appended(1)), (200, 100));

        files
            .write_at(&mut index, refs[1], 98, &[7, 7])
            .await
            .unwrap();
        for (at, within, len) in [(refs[1], 99, 2), (refs[3], 0, 1), (at(2, 0), 0, 1)] {
            let refused = files.write_at(&mut index, at, within, &vec![7; len]).await;
            assert!(is_violation(&refused), "{len} bytes at {within} of {at:?}");
        }

        let data_0 = io.file(FileRole::Data(0)).unwrap();
        let mut expected = [[0u8; 100], [1u8; 100]].concat();
        expected[198..200].copy_from_slice(&[7, 7]);
        assert_eq!(data_0.bytes().await, expected);
        assert_eq!((data_0.appends(), data_0.write_ats()), (2, 1));

        assert_eq!(files.into_writers().len(), 2);

        // An index whose frame 0 never verified: no byte of it was written for the data pages.
        assert_eq!(io.file(FileRole::Index).unwrap().write_ats(), 0);
    }

    #[tokio::test]
    async fn the_first_data_byte_after_a_completed_flush_spoils_frame_0_first_and_only_once() {
        let io = InMemoryIo::default();
        let mut files = Files::new(DataFileLayout::new(100, 200));
        let first = files.reserve_page(&io, 0).await.unwrap();
        let second = files.reserve_page(&io, 1).await.unwrap();
        let file = io.file(FileRole::Data(0)).unwrap();

        // A refusal writes nothing, and spoils nothing.
        let mut index = verifying_index(&io).await;
        let index_file = io.file(FileRole::Index).unwrap();
        assert!(is_violation(
            &files.append_page(&mut index, second, &[1; 100]).await
        ));
        assert!(is_violation(
            &files.write_at(&mut index, first, 0, &[1]).await
        ));
        assert_eq!(
            (index_file.write_ats(), index.is_frame_0_spoiled()),
            (0, false)
        );

        // The spoil fails: the page is not appended, and the next byte spoils again.
        index_file.fail_write_ats();
        assert!(matches!(
            files.append_page(&mut index, first, &[1; 100]).await,
            Err(ArchiveError::Io(_))
        ));
        assert_eq!((file.appends(), files.appended(0)), (0, 0));
        assert!(!index.is_frame_0_spoiled());

        let index_file = io.prepare(FileRole::Index);
        let mut index = verifying_index(&io).await;

        index_file.shut_writes();
        {
            let parked = files.append_page(&mut index, first, &[1; 100]);
            tokio::pin!(parked);
            tokio::select! {
                result = &mut parked => panic!("the append never parked in the spoil: {result:?}"),
                _ = wait_until("the spoil parking", || index_file.is_write_waiting()) => {}
            }
            assert_eq!(file.appends(), 0, "the page waits for the spoil");
            index_file.open_writes();
            parked.await.unwrap();
        }

        assert_eq!(index_file.bytes().await[12..20], [0xEE; 8]);
        assert!(index.is_frame_0_spoiled());

        files
            .append_page(&mut index, second, &[2; 100])
            .await
            .unwrap();
        files.write_at(&mut index, first, 5, &[9]).await.unwrap();
        assert_eq!((file.appends(), file.write_ats()), (2, 1));
        assert_eq!(
            index_file.write_ats(),
            1,
            "one spoil for every byte after it"
        );

        // Frame 0 written whole again: the next byte, a write_at, spoils it again.
        index.frame_0_verifies(SplitL0FrameWrite {
            offset: 12,
            append: false,
            bytes: vec![0xDD; 8],
        });
        files.write_at(&mut index, second, 0, &[3]).await.unwrap();
        assert_eq!(index_file.write_ats(), 2);
        assert_eq!(index_file.bytes().await[12..20], [0xDD; 8]);
    }

    #[tokio::test]
    async fn the_257th_data_file_is_refused_before_it_is_created() {
        let io = InMemoryIo::default();
        let mut files = Files::new(DataFileLayout::new(100, 100));

        for rank in 0..256 {
            files.reserve_page(&io, rank).await.unwrap();
        }

        assert!(is_violation(&files.reserve_page(&io, 256).await));
        assert_eq!(files.files(), 256);
        assert_eq!(io.created_roles().len(), 256);
    }

    #[tokio::test]
    #[should_panic(expected = "cannot change")]
    async fn the_layout_cannot_change_once_a_data_file_exists() {
        let io = InMemoryIo::default();
        let mut files = Files::new(DataFileLayout::new(100, 200));

        files.reserve_page(&io, 0).await.unwrap();
        files.set_layout(DataFileLayout::new(100, 100));
    }
}
