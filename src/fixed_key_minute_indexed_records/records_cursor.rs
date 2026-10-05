use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use crate::{
    ArchiveError, ArchiveFileReader, FixedKeyMinuteIndexedRecordsArchiveReader, IoReaderFactory,
    MinuteCells, PrimaryKeyFixedSize, RECORD_LEN_PREFIX, RecordRef, RecordsReadAhead,
    SPLIT_DATA_HEADER_LEN, frame_record, le_u32, minute_of_day,
};

/// One record a [`RecordsCursor`] walked to.
pub(crate) struct CursorRecord {
    pub time: i64,
    pub minute: usize,
    frame: Arc<Vec<u8>>,
    range: Range<usize>,
}

impl CursorRecord {
    /// The record's bytes, without their length prefix.
    pub fn bytes(&self) -> &[u8] {
        &self.frame[self.range.clone()]
    }
}

/// A walk along one key's chain of data frames, record by record, from the first record of a minute to the
/// end of the chain — checking, as it goes, everything the index says about what it walks:
///
/// * every record's time is inside the archive's day, and no earlier than the record before it;
/// * the first record of every minute the walk meets is exactly where that minute's cell points — the
///   minute the walk starts at included;
/// * the chain never returns to a frame it walked: in a working archive `next` may point back into the
///   file, when a frame was chained between two others, so a cycle is found by the frames it visits.
///
/// Frames are read through a [`RecordsReadAhead`] of its own, so the frames of a run that lies back to back
/// in its file cost a few reads rather than one each.
pub(crate) struct RecordsCursor<'a, TKey, TFactory, TReader> {
    reader: &'a FixedKeyMinuteIndexedRecordsArchiveReader<TKey, TFactory, TReader>,
    cells: &'a MinuteCells,
    cached: bool,
    read_ahead: RecordsReadAhead,
    frame: Option<Arc<Vec<u8>>>,
    at: RecordRef,
    previous: Option<(i64, usize)>,
    start_minute: usize,
    visited: HashSet<u32>,
    done: bool,
}

impl<'a, TKey, TFactory, TReader> RecordsCursor<'a, TKey, TFactory, TReader>
where
    TKey: PrimaryKeyFixedSize,
    TFactory: IoReaderFactory<TReader>,
    TReader: ArchiveFileReader,
{
    /// A walk from `first`, the record `cells` names first in `minute`. Frames come through the reader's data
    /// cache when `cached`, straight from their files otherwise.
    pub fn new(
        reader: &'a FixedKeyMinuteIndexedRecordsArchiveReader<TKey, TFactory, TReader>,
        cells: &'a MinuteCells,
        minute: usize,
        first: RecordRef,
        cached: bool,
    ) -> Self {
        Self {
            reader,
            cells,
            cached,
            read_ahead: RecordsReadAhead::new(),
            frame: None,
            at: first,
            previous: None,
            start_minute: minute,
            visited: HashSet::new(),
            done: false,
        }
    }

    /// The next record of the chain, `None` past its last.
    pub async fn next(&mut self) -> Result<Option<CursorRecord>, ArchiveError> {
        loop {
            if self.done {
                return Ok(None);
            }

            let frame = match &self.frame {
                Some(frame) => frame.clone(),
                None => self.enter_frame().await?,
            };

            let at = self.at.offset();

            if at < frame.len() {
                return self.record_at(frame, at).map(Some);
            }

            match le_u32(&frame, 4) {
                0 => self.done = true,
                next => {
                    self.at = RecordRef::new(self.at.file_no, next, SPLIT_DATA_HEADER_LEN);
                    self.frame = None;
                }
            }
        }
    }

    async fn enter_frame(&mut self) -> Result<Arc<Vec<u8>>, ArchiveError> {
        if !self.visited.insert(self.at.frame) {
            return Err(ArchiveError::Corrupted(format!(
                "the chain of data frames of data file {} returns to the frame at {}",
                self.at.file_no, self.at.frame
            )));
        }

        let frame = self.load_frame().await?;

        if self.previous.is_none() && self.at.offset() >= frame.len() {
            return Err(ArchiveError::Corrupted(format!(
                "the cell of minute {} points at {} of the data frame at {}, whose records end at {}",
                self.start_minute,
                self.at.offset,
                self.at.frame,
                frame.len()
            )));
        }

        self.frame = Some(frame.clone());
        Ok(frame)
    }

    /// The frame the walk is at: out of the reader's data cache when the walk uses it and it holds the frame,
    /// otherwise through the walk's read-ahead — and then into the cache, when the walk uses it.
    async fn load_frame(&mut self) -> Result<Arc<Vec<u8>>, ArchiveError> {
        let hit = if self.cached {
            self.reader.cached_data_frame(self.at).await
        } else {
            None
        };

        if let Some(frame) = hit {
            self.read_ahead.passed(self.at, le_u32(&frame, 0) as usize);
            return Ok(frame);
        }

        let file = self.reader.data_file(self.at.file_no).await?;
        let frame = self.read_ahead.frame(file, self.at).await?;

        if self.cached {
            return Ok(self.reader.cache_data_frame(self.at, frame).await);
        }

        Ok(Arc::new(frame))
    }

    fn record_at(&mut self, frame: Arc<Vec<u8>>, at: usize) -> Result<CursorRecord, ArchiveError> {
        let (len, time) = frame_record(&frame, at, frame.len(), self.at.frame)?;

        let day_start = self.reader.day_start().unix_microseconds;

        let Some(minute) = minute_of_day(day_start, time) else {
            return Err(ArchiveError::Corrupted(format!(
                "the record at {at} of the data frame at {} is at {time} µs, outside the archive's day \
                 starting at {day_start} µs",
                self.at.frame
            )));
        };

        let starts_a_minute = match self.previous {
            None => {
                if minute != self.start_minute {
                    return Err(self.misplaced(self.start_minute));
                }

                true
            }
            Some((previous_time, previous_minute)) => {
                if time < previous_time {
                    return Err(ArchiveError::Corrupted(format!(
                        "the record at {at} of the data frame at {} is earlier than the one before it",
                        self.at.frame
                    )));
                }

                minute != previous_minute
            }
        };

        if starts_a_minute && self.cells.get(minute) != Some(self.at) {
            return Err(self.misplaced(minute));
        }

        self.previous = Some((time, minute));
        self.at = RecordRef::new(self.at.file_no, self.at.frame, at + len);

        Ok(CursorRecord {
            time,
            minute,
            frame,
            range: at + RECORD_LEN_PREFIX..at + len,
        })
    }

    fn misplaced(&self, minute: usize) -> ArchiveError {
        ArchiveError::Corrupted(format!(
            "the cell of minute {minute} does not point at its first record, at {} of the data frame at {} \
             of data file {}",
            self.at.offset, self.at.frame, self.at.file_no
        ))
    }
}
