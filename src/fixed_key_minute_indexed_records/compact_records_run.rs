use crate::{DATA_FRAME_CAPACITY, RECORD_LEN_PREFIX, SPLIT_DATA_HEADER_LEN};

/// Where the records of one key lie in a COMPACT archive: `len` bytes of frames back to back, holding
/// `records` records, and the first record of every minute that has one, as `(minute, frame, offset)` — the
/// frame's offset from the start of the run, and the record's offset from the start of its frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompactRecordsRun {
    pub len: u64,
    pub records: u64,
    pub cells: Vec<(u16, u32, u32)>,
}

/// Cuts one key's records, handed over in chain order, into the frames of the compact form: each frame
/// filled while the next record fits [`DATA_FRAME_CAPACITY`], no free space after its records, and chained
/// to the frame right after it — `next` 0 on the last. The same records always cut the same way, whatever
/// frames they came from.
///
/// Built with a `run_start` it keeps the bytes and hands back every frame as it closes; without one it only
/// measures, which is what a plan needs.
pub(crate) struct CompactRecordsCutter {
    run: CompactRecordsRun,
    frame_start: u64,
    used: usize,
    last_minute: Option<usize>,
    bytes: Option<(u32, Vec<u8>)>,
}

impl CompactRecordsCutter {
    pub fn measuring() -> Self {
        Self::new(None)
    }

    /// The frames of a run starting at `run_start` of its data file.
    pub fn writing(run_start: u32) -> Self {
        Self::new(Some(run_start))
    }

    fn new(run_start: Option<u32>) -> Self {
        Self {
            run: CompactRecordsRun::default(),
            frame_start: 0,
            used: 0,
            last_minute: None,
            bytes: run_start.map(|start| (start, Vec::new())),
        }
    }

    /// The next record of `minute`; the frame it closed, when it did not fit the current one and bytes are
    /// kept.
    pub fn push(&mut self, minute: usize, record: &[u8]) -> Option<Vec<u8>> {
        let len = RECORD_LEN_PREFIX + record.len();
        let mut closed = None;

        if self.used > 0 && self.used + len > DATA_FRAME_CAPACITY {
            let next = self.frame_start + (SPLIT_DATA_HEADER_LEN + self.used) as u64;
            closed = self.close_frame(next);
            self.frame_start = next;
            self.used = 0;
        }

        if self.last_minute != Some(minute) {
            self.run.cells.push((
                minute as u16,
                self.frame_start as u32,
                (SPLIT_DATA_HEADER_LEN + self.used) as u32,
            ));
            self.last_minute = Some(minute);
        }

        if let Some((_, frame)) = &mut self.bytes {
            frame.extend_from_slice(&(record.len() as u16).to_le_bytes());
            frame.extend_from_slice(record);
        }

        self.used += len;
        self.run.records += 1;
        closed
    }

    /// The run, and its last frame when bytes are kept and it has one.
    pub fn finish(mut self) -> (CompactRecordsRun, Option<Vec<u8>>) {
        if self.used == 0 {
            return (self.run, None);
        }

        self.run.len = self.frame_start + (SPLIT_DATA_HEADER_LEN + self.used) as u64;
        let last = self.close_frame_last();
        (self.run, last)
    }

    fn close_frame(&mut self, next_in_run: u64) -> Option<Vec<u8>> {
        let (run_start, _) = self.bytes.as_ref()?;
        let next = *run_start as u64 + next_in_run;
        self.encode_frame(next as u32)
    }

    fn close_frame_last(&mut self) -> Option<Vec<u8>> {
        self.encode_frame(0)
    }

    fn encode_frame(&mut self, next: u32) -> Option<Vec<u8>> {
        let (_, records) = self.bytes.as_mut()?;

        let mut frame = Vec::with_capacity(SPLIT_DATA_HEADER_LEN + records.len());
        frame.extend_from_slice(&((SPLIT_DATA_HEADER_LEN + records.len()) as u32).to_le_bytes());
        frame.extend_from_slice(&next.to_le_bytes());
        frame.extend_from_slice(&(records.len() as u16).to_le_bytes());
        frame.append(records);
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::le_u32;

    #[test]
    fn records_are_cut_into_frames_filled_while_the_next_record_fits() {
        // 4 records of 8 002 bytes fill 32 008 of 32 758; the fifth starts a frame.
        let big = vec![0xAB; 8_000];
        let mut measuring = CompactRecordsCutter::measuring();
        let mut writing = CompactRecordsCutter::writing(1_000);
        let mut frames = Vec::new();

        for (index, minute) in [0, 0, 5, 5, 5, 9].into_iter().enumerate() {
            assert_eq!(measuring.push(minute, &big), None);
            frames.extend(writing.push(minute, &big));
            assert_eq!(frames.len(), usize::from(index >= 4), "record {index}");
        }

        let (measured, none) = measuring.finish();
        let (written, last) = writing.finish();
        assert_eq!(none, None);
        assert_eq!(measured, written);
        frames.extend(last);

        assert_eq!(
            measured,
            CompactRecordsRun {
                len: (10 + 4 * 8_002) + (10 + 2 * 8_002),
                records: 6,
                cells: vec![(0, 0, 10), (5, 0, 10 + 2 * 8_002), (9, 32_018, 10 + 8_002)],
            }
        );

        assert_eq!(frames[0].len(), 32_018);
        assert_eq!(
            (le_u32(&frames[0], 0), le_u32(&frames[0], 4)),
            (32_018, 1_000 + 32_018)
        );
        assert_eq!(&frames[0][8..10], &(4u16 * 8_002).to_le_bytes());
        assert_eq!((le_u32(&frames[1], 0), le_u32(&frames[1], 4)), (16_014, 0));
    }

    #[test]
    fn no_record_is_an_empty_run_with_no_frame() {
        let (run, last) = CompactRecordsCutter::writing(0).finish();
        assert_eq!((run, last), (CompactRecordsRun::default(), None));
    }
}
