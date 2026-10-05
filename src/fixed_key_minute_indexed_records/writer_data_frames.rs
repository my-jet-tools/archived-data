use crate::{
    AppendFrontier, ArchiveError, ArchiveFileReader, ArchiveFileWriter, DATA_FRAME_LEN,
    DEFAULT_WRITER_DATA_CACHE_PAGES, IoReaderFactory, IoWriterFactory, MAX_DATA_FILE_BYTES,
    RecordsFrame, SplitIndexWriter, WriterPageCache,
};

/// The data file of a WORKING records archive and the frames a writer holds of it.
///
/// The working form writes every frame of the day into ONE data file, number 0, every frame
/// [`DATA_FRAME_LEN`] bytes, so a frame's offset is a multiple of it. The file is created with the first
/// frame, and grows only by append: frames are reserved ahead of their bytes, and **a frame past the append
/// frontier is always cached** — a frame leaving the cache past it appends every frame before it first. A
/// frame the cache does not hold is read back through a reader of the same file, created on first need.
///
/// Every byte reaching the file spoils frame 0's hash in the archive's index first (see
/// [`SplitIndexWriter::spoil_frame_0`]).
pub(crate) struct RecordsWriterDataFrames<TWriter, TReader> {
    writer: Option<TWriter>,
    reader: Option<TReader>,
    frontier: AppendFrontier,
    max_bytes: u64,
    cache_size: usize,
    frames: WriterPageCache<RecordsFrame>,
}

impl<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>
    RecordsWriterDataFrames<TWriter, TReader>
{
    pub fn new() -> Self {
        Self {
            writer: None,
            reader: None,
            frontier: AppendFrontier::new(0),
            max_bytes: MAX_DATA_FILE_BYTES,
            cache_size: DEFAULT_WRITER_DATA_CACHE_PAGES,
            frames: WriterPageCache::new(),
        }
    }

    pub fn set_cache_size(&mut self, frames: usize) {
        self.cache_size = frames;
    }

    /// A limit below 4 GiB, so a test reaches it with a few frames.
    #[cfg(test)]
    pub fn set_max_bytes(&mut self, bytes: u64) {
        self.max_bytes = bytes.min(MAX_DATA_FILE_BYTES);
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// How far frames have been reserved: the size the file will have.
    pub fn reserved(&self) -> u64 {
        self.frontier.reserved()
    }

    /// Whether `frames` more frames fit below the limit.
    pub fn has_room_for(&self, frames: usize) -> bool {
        self.frontier.reserved() + (frames * DATA_FRAME_LEN) as u64 <= self.max_bytes
    }

    /// Creates data file 0, once.
    pub async fn create<TFactory: IoWriterFactory<TWriter>>(
        &mut self,
        factory: &TFactory,
    ) -> Result<(), ArchiveError> {
        if self.writer.is_none() {
            self.writer = Some(factory.create_data_writer(0).await?);
        }

        Ok(())
    }

    /// A new empty frame at the end of the file, cached as dirty. The caller created the file and checked
    /// the room.
    pub fn allocate(&mut self) -> u32 {
        debug_assert!(self.writer.is_some() && self.has_room_for(1));

        let offset = self.frontier.reserve(DATA_FRAME_LEN);
        self.frames
            .insert(offset, RecordsFrame::new(DATA_FRAME_LEN), true);
        offset as u32
    }

    /// Caches the frame at `offset`, read back from the file when the cache does not hold it; evicts
    /// nothing. An offset that is not a frame the file holds is `Corrupted`.
    pub async fn load<TFactory: IoReaderFactory<TReader>>(
        &mut self,
        factory: &TFactory,
        offset: u32,
    ) -> Result<(), ArchiveError> {
        if self.frames.touch(offset as u64) {
            return Ok(());
        }

        let is_frame = (offset as usize).is_multiple_of(DATA_FRAME_LEN)
            && (offset as u64) < self.frontier.reserved();

        if !is_frame {
            return Err(ArchiveError::Corrupted(format!(
                "the index points at a data frame at {offset}; the working data file holds frames of \
                 {DATA_FRAME_LEN} bytes up to {}",
                self.frontier.reserved()
            )));
        }

        if !self.frontier.is_appended(offset as u64) {
            return Err(ArchiveError::ContractViolation(format!(
                "the data frame at {offset} is past the end of the data file and not cached: \
                 the writer is not usable after an earlier error"
            )));
        }

        if self.reader.is_none() {
            self.reader = Some(factory.create_data_reader(0).await?);
        }

        let reader = self.reader.as_ref().unwrap();
        let bytes = reader.read_at(offset as u64, DATA_FRAME_LEN).await?;
        let frame = RecordsFrame::decode(bytes, offset)?;

        self.frames.insert(offset as u64, frame, false);
        Ok(())
    }

    /// A cached frame. Panics when it is not cached: the writer loads every frame it reads first.
    pub fn frame(&self, offset: u32) -> &RecordsFrame {
        self.frames
            .get(offset as u64)
            .expect("a data frame in use is cached")
    }

    /// A cached frame to change, marked dirty. Panics when it is not cached.
    pub fn modify(&mut self, offset: u32) -> &mut RecordsFrame {
        self.frames
            .modify(offset as u64)
            .expect("a data frame in use is cached")
    }

    /// Stores and drops the least recently used frames until the cache is within its size.
    pub async fn evict(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
    ) -> Result<(), ArchiveError> {
        while self.frames.len() > self.cache_size {
            let Some(oldest) = self.frames.least_recently_used() else {
                break;
            };

            self.store(index, oldest).await?;
            self.frames.remove(oldest);
        }

        Ok(())
    }

    /// Every dirty frame, in file order.
    pub async fn flush(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
    ) -> Result<(), ArchiveError> {
        for offset in self.frames.dirty_offsets() {
            self.store(index, offset).await?;
        }

        Ok(())
    }

    pub fn into_writer(self) -> Option<TWriter> {
        self.writer
    }

    async fn store(
        &mut self,
        index: &mut SplitIndexWriter<TWriter, TReader>,
        offset: u64,
    ) -> Result<(), ArchiveError> {
        if !self.frames.is_dirty(offset) {
            return Ok(());
        }

        let writer = self
            .writer
            .as_ref()
            .expect("a frame is allocated only once the data file exists");

        if self.frontier.is_appended(offset) {
            index.spoil_frame_0().await?;
            writer
                .write_at(offset, self.frame(offset as u32).bytes())
                .await?;
            self.frames.mark_clean(offset);
            return Ok(());
        }

        let mut run = self.frontier.appended();

        while run <= offset {
            let Some(frame) = self.frames.get(run) else {
                return Err(ArchiveError::ContractViolation(format!(
                    "the data frame at {run} is past the end of the data file and not cached: \
                     the writer is not usable after an earlier error"
                )));
            };

            self.frontier.check_append(run, DATA_FRAME_LEN)?;
            index.spoil_frame_0().await?;
            let returned = writer.append(frame.bytes()).await?;
            self.frontier.confirm_append(returned, DATA_FRAME_LEN)?;
            self.frames.mark_clean(run);
            run += DATA_FRAME_LEN as u64;
        }

        Ok(())
    }
}
