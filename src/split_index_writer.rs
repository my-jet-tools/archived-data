use crate::{
    AppendFrontier, ArchiveError, ArchiveFileReader, ArchiveFileWriter, IoReaderFactory,
    IoWriterFactory, MAX_INDEX_BYTES, SplitL0FrameWrite, reserve_index_bytes,
};

/// The index file of an archive being written. Offsets are reserved ahead of their bytes and every byte
/// reaches the file by an append at the frontier; bytes already appended are rewritten in place, and read
/// back through a reader of the same file, created on first need.
///
/// It also keeps the one fact that says whether the index opens: whether frame 0's hash verifies in the
/// file. From the moment a flush writes frame 0 whole to the first byte after it — of the index or of any
/// data file — it does, so that byte is preceded by [`Self::spoil_frame_0`], which spoils the hash in place.
/// Every later byte finds it spoiled and costs nothing more, until the next flush writes frame 0 again.
pub(crate) struct SplitIndexWriter<TWriter, TReader> {
    writer: TWriter,
    reader: Option<TReader>,
    frontier: AppendFrontier,
    max_bytes: u64,
    /// `Some` while frame 0's hash verifies in the file: the write that spoils it.
    frame_0_spoil: Option<SplitL0FrameWrite>,
}

impl<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader> SplitIndexWriter<TWriter, TReader> {
    /// Creates the index and appends `frame_0` at offset 0: a frame 0 whose hash does NOT verify, so nothing
    /// is spoiled before the first flush.
    pub async fn create<TFactory: IoWriterFactory<TWriter>>(
        factory: &TFactory,
        frame_0: &[u8],
    ) -> Result<Self, ArchiveError> {
        let mut index = Self {
            writer: factory.create_index_writer().await?,
            reader: None,
            frontier: AppendFrontier::new(0),
            max_bytes: MAX_INDEX_BYTES,
            frame_0_spoil: None,
        };

        let offset = index.reserve(frame_0.len())?;
        index.append(offset, frame_0).await?;
        Ok(index)
    }

    /// The offset `len` more bytes will start at; an index that would grow past 4 GiB is refused.
    pub fn reserve(&mut self, len: usize) -> Result<u32, ArchiveError> {
        let offset = self.check_reservable(len)?;
        self.frontier.reserve(len);
        Ok(offset)
    }

    /// What [`Self::reserve`] would answer, reserving nothing: a writer asks before it changes anything that
    /// a refused reservation would leave half done.
    pub fn check_reservable(&self, len: usize) -> Result<u32, ArchiveError> {
        let end = self.frontier.reserved();
        let (offset, grown) = reserve_index_bytes(end, len)?;

        if grown > self.max_bytes {
            return Err(ArchiveError::ContractViolation(format!(
                "{len} more bytes at {end} would grow the index past {} bytes",
                self.max_bytes
            )));
        }

        Ok(offset)
    }

    /// A limit below 4 GiB, so a test reaches it with a few pages.
    #[cfg(test)]
    pub fn set_max_bytes(&mut self, bytes: u64) {
        self.max_bytes = bytes.min(MAX_INDEX_BYTES);
    }

    pub fn is_appended(&self, offset: u32) -> bool {
        self.frontier.is_appended(offset as u64)
    }

    pub fn appended(&self) -> u64 {
        self.frontier.appended()
    }

    pub fn reserved(&self) -> u64 {
        self.frontier.reserved()
    }

    /// `offset` must be the frontier: bytes reach the index in the order their offsets were reserved. Frame
    /// 0's hash is spoiled first, if it verifies.
    pub async fn append(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ArchiveError> {
        self.frontier.check_append(offset as u64, bytes.len())?;
        self.spoil_frame_0().await?;
        let returned = self.writer.append(bytes).await?;
        self.frontier.confirm_append(returned, bytes.len())
    }

    /// Frame 0's hash is spoiled first, if it verifies.
    pub async fn write_at(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ArchiveError> {
        self.check_appended(offset, bytes.len())?;
        self.spoil_frame_0().await?;
        Ok(self.writer.write_at(offset as u64, bytes).await?)
    }

    /// Spoils frame 0's hash in place — one `write_at` of 8 bytes — when it verifies, and does nothing when
    /// it does not. Called BEFORE every byte reaches the index or a data file, so the first byte written
    /// after a completed flush lands in an index already refused at open. It counts as spoiled only once
    /// the `write_at` returned `Ok`: one that failed or was dropped is repeated by the next byte.
    pub async fn spoil_frame_0(&mut self) -> Result<(), ArchiveError> {
        let Some(spoil) = &self.frame_0_spoil else {
            return Ok(());
        };

        self.check_appended(spoil.offset, spoil.bytes.len())?;
        self.writer
            .write_at(spoil.offset as u64, &spoil.bytes)
            .await?;

        self.frame_0_spoil = None;
        Ok(())
    }

    /// A flush just wrote frame 0 whole, with its real hash: the index opens until the next byte, which
    /// `spoil` — the write that spoils THIS frame 0's hash — precedes.
    pub fn frame_0_verifies(&mut self, spoil: SplitL0FrameWrite) {
        self.frame_0_spoil = Some(spoil);
    }

    /// Whether frame 0's hash is spoiled: from the index's creation until a flush completes, and from the
    /// first byte written after it until the next one does.
    pub fn is_frame_0_spoiled(&self) -> bool {
        self.frame_0_spoil.is_none()
    }

    /// Exactly `len` appended bytes. Not through a `SizedReader`: the index grows while it is read back.
    pub async fn read_back<TFactory: IoReaderFactory<TReader>>(
        &mut self,
        factory: &TFactory,
        offset: u32,
        len: usize,
    ) -> Result<Vec<u8>, ArchiveError> {
        self.check_appended(offset, len)?;

        if self.reader.is_none() {
            self.reader = Some(factory.create_index_reader().await?);
        }

        let reader = self.reader.as_ref().unwrap();
        Ok(reader.read_at(offset as u64, len).await?)
    }

    pub fn into_writer(self) -> TWriter {
        self.writer
    }

    fn check_appended(&self, offset: u32, len: usize) -> Result<(), ArchiveError> {
        if offset as u64 + len as u64 > self.frontier.appended() {
            return Err(ArchiveError::ContractViolation(format!(
                "{len} index bytes at {offset} are not all appended; the index is appended up to {}",
                self.frontier.appended()
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
    use crate::{
        ArchiveError, ArchiveFileWriter, MAX_INDEX_BYTES, SplitIndexWriter, SplitL0FrameWrite,
        l0_frame,
    };

    type Index = SplitIndexWriter<InMemoryWriter, InMemoryReader>;

    fn is_violation<T>(result: &Result<T, ArchiveError>) -> bool {
        matches!(result, Err(ArchiveError::ContractViolation(_)))
    }

    #[tokio::test]
    async fn bytes_reach_the_index_in_reservation_order() {
        let io = InMemoryIo::default();
        let frame_0 = l0_frame(40, 0, 0, &[]);
        let mut index = Index::create(&io, &frame_0).await.unwrap();

        let file = io.file(FileRole::Index).unwrap();
        assert_eq!(file.bytes().await, frame_0);
        assert_eq!((index.appended(), index.reserved()), (40, 40));

        let first = index.reserve(8).unwrap();
        let second = index.reserve(8).unwrap();
        assert_eq!((first, second), (40, 48));
        assert!(!index.is_appended(first));

        assert!(is_violation(&index.append(second, &[2; 8]).await));
        assert_eq!(file.appends(), 1);

        index.append(first, &[1; 8]).await.unwrap();
        index.append(second, &[2; 8]).await.unwrap();
        assert!(index.is_appended(second));
        assert_eq!(file.bytes().await[40..], [[1; 8], [2; 8]].concat());
    }

    #[tokio::test]
    async fn only_appended_bytes_are_rewritten_or_read_back() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &l0_frame(20, 0, 0, &[])).await.unwrap();
        let file = io.file(FileRole::Index).unwrap();

        let page = index.reserve(8).unwrap();
        index.append(page, &[1; 8]).await.unwrap();
        let pending = index.reserve(8).unwrap();

        index.write_at(page, &[9; 8]).await.unwrap();
        assert!(is_violation(&index.write_at(page, &[9; 9]).await));
        assert!(is_violation(&index.write_at(pending, &[9; 1]).await));
        assert_eq!(file.write_ats(), 1);

        assert_eq!(file.readers_opened(), 0);
        assert_eq!(index.read_back(&io, page, 8).await.unwrap(), [9; 8]);
        assert_eq!(index.read_back(&io, 0, 4).await.unwrap(), [20, 0, 0, 0]);
        assert_eq!(file.readers_opened(), 1);

        assert!(is_violation(&index.read_back(&io, page, 9).await));
        assert!(is_violation(&index.read_back(&io, pending, 8).await));
        assert_eq!(file.eof_reads(), 0);

        index.into_writer().finish().await.unwrap();
        assert_eq!(file.finishes(), 1);
    }

    #[tokio::test]
    async fn the_index_is_reserved_up_to_four_gib_and_no_further() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &l0_frame(20, 0, 0, &[])).await.unwrap();

        assert_eq!(index.reserve((MAX_INDEX_BYTES - 21) as usize).unwrap(), 20);
        assert!(is_violation(&index.reserve(2)));
        assert_eq!(index.reserve(1).unwrap(), u32::MAX);
        assert_eq!(index.reserved(), MAX_INDEX_BYTES);
        assert!(is_violation(&index.reserve(0)));
    }

    #[tokio::test]
    async fn a_lowered_limit_refuses_like_four_gib_and_a_check_reserves_nothing() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &l0_frame(20, 0, 0, &[])).await.unwrap();
        index.set_max_bytes(50);

        assert_eq!(index.check_reservable(30).unwrap(), 20);
        assert!(is_violation(&index.check_reservable(31)));
        assert_eq!(index.reserved(), 20);

        assert_eq!(index.reserve(20).unwrap(), 20);
        assert!(is_violation(&index.reserve(11)));
        assert_eq!(index.reserve(10).unwrap(), 40);
        assert!(is_violation(&index.check_reservable(1)));
        assert_eq!(index.reserved(), 50);
    }

    fn spoil(byte: u8) -> SplitL0FrameWrite {
        SplitL0FrameWrite {
            offset: 12,
            append: false,
            bytes: vec![byte; 8],
        }
    }

    #[tokio::test]
    async fn a_frame_0_that_verifies_is_spoiled_before_the_next_byte_and_only_once() {
        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &[0; 20]).await.unwrap();
        let file = io.file(FileRole::Index).unwrap();

        // Created with a frame 0 that does not verify: nothing to spoil.
        assert!(index.is_frame_0_spoiled());
        let page = index.reserve(4).unwrap();
        index.append(page, &[1; 4]).await.unwrap();
        index.write_at(page, &[2; 4]).await.unwrap();
        index.spoil_frame_0().await.unwrap();
        assert_eq!((file.appends(), file.write_ats()), (2, 1));

        // A flush wrote frame 0 whole. A refusal spoils nothing.
        index.frame_0_verifies(spoil(0xA1));
        assert!(!index.is_frame_0_spoiled());
        let next = index.reserve(4).unwrap();
        assert!(is_violation(&index.append(next + 4, &[3; 4]).await));
        assert!(is_violation(&index.write_at(next, &[3; 4]).await));
        assert_eq!((file.appends(), file.write_ats()), (2, 1));

        // The spoil fails: the append is not made, and the next byte spoils again.
        file.fail_write_ats_after(0);
        assert!(matches!(
            index.append(next, &[3; 4]).await,
            Err(ArchiveError::Io(_))
        ));
        assert_eq!((file.appends(), index.appended()), (2, 24));
        assert!(!index.is_frame_0_spoiled());

        let io = InMemoryIo::default();
        let mut index = Index::create(&io, &[0; 20]).await.unwrap();
        let file = io.file(FileRole::Index).unwrap();
        let first = index.reserve(4).unwrap();
        let second = index.reserve(4).unwrap();
        index.append(first, &[1; 4]).await.unwrap();
        index.frame_0_verifies(spoil(0xA1));

        // The first byte after it, an append, is preceded by the spoil; the bytes after it are not.
        index.append(second, &[2; 4]).await.unwrap();
        assert_eq!(file.bytes().await[12..20], [0xA1; 8]);
        assert!(index.is_frame_0_spoiled());
        index.write_at(first, &[5; 4]).await.unwrap();
        index.spoil_frame_0().await.unwrap();
        assert_eq!((file.appends(), file.write_ats()), (3, 2));
        assert_eq!(file.write_ats_of(12, 8), 1);

        // Frame 0 written whole again, then a `write_at` first: spoiled with what THIS frame 0 needs.
        index.write_at(0, &[0; 20]).await.unwrap();
        index.frame_0_verifies(spoil(0xB2));
        index.write_at(second, &[6; 4]).await.unwrap();
        index.write_at(second, &[7; 4]).await.unwrap();
        assert_eq!(file.write_ats_of(12, 8), 2);
        assert_eq!(file.bytes().await[12..20], [0xB2; 8]);
        assert_eq!(file.bytes().await[20..], [[5; 4], [7; 4]].concat());
    }
}
