use rust_extensions::date_time::DateTimeAsMicroseconds;

use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, DATA_FRAME_LEN, DEFAULT_L0_CAPACITY,
    DEFAULT_L1_CAPACITY, InFlightGuard, IoReaderFactory, IoWriterFactory, PrimaryKeyFixedSize,
    RecordsWriterDataFrames, RecordsWriterIndexPages, SplitIndexWriter, SplitL0Interval,
    SplitWorkingL0Chain, check_record_len, check_records_type_sizes, encode_split_l0_bodies,
    format_microseconds, minute_of_day, record_time, records_l1_page_len, split_l0_frame_len,
};

/// Builds a WORKING day archive of records — an index and ONE data file — from records written in any
/// order, through the writers (and, to read its own pages back, the readers) of one factory.
///
/// A key is a fixed-size account; a record is its bytes, whose first 8 are the event's time as an `i64`
/// little-endian of microseconds. Every record of a key is kept in time order in one chain of data frames of
/// [`DATA_FRAME_LEN`] bytes, a record of an equal time after those already written; the key's L2 frame in
/// the index points at the first record of every minute of the day. The first record of a key allocates its
/// L2 frame and its first data frame. The index holds L0 frame 0 at offset 0, appended when the writer is
/// created with a hash that does not verify, then L1 pages of `capacity_l1` keys and L2 frames in the order
/// they were allocated, and at flush the L0 continuations it needs.
///
/// A frame that overflows follows the crate's one rule. A record after the last one of a full frame goes
/// into a new frame chained after it; a record before a record of a full frame halves the frame at the
/// record boundary nearest its middle, and the upper half moves in front of the next frame's records when it
/// fits there, into a new frame chained between the two when it does not. The L2 cells follow every record
/// that moves.
///
/// **A record outside the writer's day PANICS**, and so does a data frame that would take the data file past
/// 4 GiB: both name the archive. A record shorter than its time or longer than [`crate::MAX_RECORD_LEN`], a
/// key that does not encode, and a key needing index room past 4 GiB are refused, change nothing, and leave
/// the writer usable.
///
/// Everything else is the split working archive's contract (see [`crate::ArchiveFileWriter`]): files grow
/// only by append; **nothing written is readable until [`Self::flush`] or [`Self::finish`] returns `Ok`**;
/// the index opens only when its last flush completed and nothing was written to its files after it; any
/// other `Err`, and a `write` or `flush` future dropped before it returned, leaves every later call refused.
pub struct FixedKeyMinuteIndexedRecordsArchiveWriter<TKey, TFactory, TWriter, TReader> {
    pub(crate) factory: TFactory,
    name: String,
    day_start: DateTimeAsMicroseconds,
    pub(crate) index: SplitIndexWriter<TWriter, TReader>,
    pub(crate) capacity_l0: usize,
    pub(crate) capacity_l1: usize,
    pub(crate) l0: Vec<SplitL0Interval<TKey>>,
    pub(crate) l0_chain: SplitWorkingL0Chain,
    pub(crate) keys: usize,
    records: u64,
    pub(crate) index_pages: RecordsWriterIndexPages<TKey>,
    pub(crate) data: RecordsWriterDataFrames<TWriter, TReader>,
    in_flight: InFlightGuard,
}

impl<TKey, TFactory, TWriter, TReader>
    FixedKeyMinuteIndexedRecordsArchiveWriter<TKey, TFactory, TWriter, TReader>
where
    TKey: PrimaryKeyFixedSize,
    TFactory: IoWriterFactory<TWriter> + IoReaderFactory<TReader>,
    TWriter: ArchiveFileWriter,
    TReader: ArchiveFileReader,
{
    /// [`DEFAULT_L0_CAPACITY`] intervals per L0 frame, [`DEFAULT_L1_CAPACITY`] keys per L1 page.
    pub async fn new(
        factory: TFactory,
        name: impl Into<String>,
        day_start: DateTimeAsMicroseconds,
    ) -> Result<Self, ArchiveError> {
        Self::new_with_capacity(
            factory,
            name,
            day_start,
            DEFAULT_L0_CAPACITY,
            DEFAULT_L1_CAPACITY,
        )
        .await
    }

    /// An archive of the day of 1440 minutes starting at `day_start`, called `name` in every panic. Creates
    /// the index and appends frame 0; data file 0 is created with the first record.
    ///
    /// Panics when a capacity is zero or a page of this key cannot fit its `u32` size.
    pub async fn new_with_capacity(
        factory: TFactory,
        name: impl Into<String>,
        day_start: DateTimeAsMicroseconds,
        capacity_l0: usize,
        capacity_l1: usize,
    ) -> Result<Self, ArchiveError> {
        check_records_type_sizes::<TKey>(capacity_l0, capacity_l1);

        let l0_chain = SplitWorkingL0Chain::new(split_l0_frame_len::<TKey>(capacity_l0));
        let index = SplitIndexWriter::create(&factory, &l0_chain.unflushed_frame()?).await?;

        Ok(Self {
            factory,
            name: name.into(),
            day_start,
            index,
            capacity_l0,
            capacity_l1,
            l0: Vec::new(),
            l0_chain,
            keys: 0,
            records: 0,
            index_pages: RecordsWriterIndexPages::new(capacity_l1),
            data: RecordsWriterDataFrames::new(),
            in_flight: InFlightGuard::new(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn day_start(&self) -> DateTimeAsMicroseconds {
        self.day_start
    }

    /// At least 1. A smaller cache only costs IO.
    pub fn set_l1_cache_size(&mut self, pages: usize) {
        self.index_pages.set_l1_cache_size(pages);
    }

    /// 0 keeps no L2 frame between writes.
    pub fn set_l2_cache_size(&mut self, frames: usize) {
        self.index_pages.set_l2_cache_size(frames);
    }

    /// 0 keeps no data frame between writes: every frame a write changed reaches the data file before it
    /// returns.
    pub fn set_data_cache_size(&mut self, frames: usize) {
        self.data.set_cache_size(frames);
    }

    pub fn keys_count(&self) -> usize {
        self.keys
    }

    pub fn records_count(&self) -> u64 {
        self.records
    }

    pub fn l0_intervals(&self) -> usize {
        self.l0.len()
    }

    /// The L0 frames the index has room for so far: frame 0 plus every continuation a flush reserved.
    pub fn l0_frames(&self) -> usize {
        self.l0_chain.frames()
    }

    /// How many bytes the data file has reserved for frames: the size it has once flushed.
    pub fn data_file_bytes(&self) -> u64 {
        self.data.reserved()
    }

    /// Writes `record` of `key` into its place in time order.
    ///
    /// Panics when the record's time is outside the archive's day, or when a data frame it needs would take
    /// the data file past 4 GiB — on a usable writer: one an earlier call left unusable refuses first.
    pub async fn write(&mut self, key: TKey, record: &[u8]) -> Result<(), ArchiveError> {
        self.in_flight.refuse_if_in_flight()?;
        check_record_len(record)?;
        let time = record_time(record);
        let minute = self.minute_of(time);

        self.in_flight.begin()?;

        let written = match self.l2_offset_of(&key).await? {
            Some(l2_offset) => self.insert_record(l2_offset, minute, time, record).await,
            None => match self.place_new_key(&key).await? {
                Ok(placement) => {
                    self.insert_first_record(key, placement, minute, record)
                        .await
                }
                Err(refused) => {
                    self.evict().await?;
                    self.in_flight.complete();
                    return Err(refused);
                }
            },
        };

        written?;
        self.evict().await?;
        self.records += 1;
        self.in_flight.complete();
        Ok(())
    }

    /// Every cached L1 page and L2 frame reaches the index in offset order, every cached data frame the data
    /// file, then the L0 chain is laid out — the continuations it has, new ones appended at the end — and
    /// frame 0 is written LAST, as the split working archive's flush does. With nothing written since the
    /// last flush, a flush writes nothing at all.
    pub async fn flush(&mut self) -> Result<(), ArchiveError> {
        self.in_flight.begin()?;
        let flushed = self.flush_in_flight().await;
        self.in_flight.complete_if_ok(flushed)
    }

    /// [`Self::flush`], then `finish` on the index and on the data file, each awaited even after the other
    /// failed. The flush's error comes first, then the first failed finish.
    pub async fn finish(mut self) -> Result<(), ArchiveError> {
        let flushed = self.flush().await;

        let mut finished = self.index.into_writer().finish().await;

        if let Some(data) = self.data.into_writer() {
            let result = data.finish().await;

            if finished.is_ok() {
                finished = result;
            }
        }

        flushed?;
        Ok(finished?)
    }

    /// A limit on the data file below 4 GiB, so a test reaches it with a few frames.
    #[cfg(test)]
    pub(crate) fn set_max_data_file_bytes(&mut self, bytes: u64) {
        self.data.set_max_bytes(bytes);
    }

    /// A limit on the index below 4 GiB, so a test reaches it with a few pages.
    #[cfg(test)]
    pub(crate) fn set_max_index_bytes(&mut self, bytes: u64) {
        self.index.set_max_bytes(bytes);
    }

    async fn flush_in_flight(&mut self) -> Result<(), ArchiveError> {
        self.index_pages.flush(&mut self.index).await?;

        if self.index.appended() != self.index.reserved() {
            return Err(ArchiveError::ContractViolation(format!(
                "the index is reserved up to {} and appended only up to {}: \
                 the writer is not usable after an earlier error",
                self.index.reserved(),
                self.index.appended()
            )));
        }

        self.data.flush(&mut self.index).await?;

        if !self.index.is_frame_0_spoiled() {
            return Ok(());
        }

        let bodies = encode_split_l0_bodies(&self.l0, self.capacity_l0)?;
        let writes = self
            .l0_chain
            .lay_out(bodies, |len| self.index.reserve(len))?;

        for write in writes {
            if write.append {
                self.index.append(write.offset, &write.bytes).await?;
            } else {
                self.index.write_at(write.offset, &write.bytes).await?;
            }
        }

        self.index.frame_0_verifies(self.l0_chain.invalidation());
        Ok(())
    }

    /// Stores what the caches hold past their sizes, once a write no longer needs the pages it read in.
    pub(crate) async fn evict(&mut self) -> Result<(), ArchiveError> {
        self.index_pages.evict(&mut self.index).await?;
        self.data.evict(&mut self.index).await
    }

    fn minute_of(&self, time: i64) -> usize {
        let day_start = self.day_start.unix_microseconds;

        minute_of_day(day_start, time).unwrap_or_else(|| {
            panic!(
                "archive {}: a record at {} ({time} µs) is outside its day, which starts at {} ({day_start} µs)",
                self.name,
                format_microseconds(time),
                format_microseconds(day_start),
            )
        })
    }

    /// Panics when `frames` more data frames would take the data file past its limit.
    pub(crate) fn check_data_room(&self, frames: usize) {
        if !self.data.has_room_for(frames) {
            panic!(
                "archive {}: its data file is {} bytes, and {} more for a data frame would take it past {} bytes",
                self.name,
                self.data.reserved(),
                frames * DATA_FRAME_LEN,
                self.data.max_bytes()
            );
        }
    }

    pub(crate) fn l1_page_len(&self) -> usize {
        records_l1_page_len::<TKey>(self.capacity_l1)
    }
}
