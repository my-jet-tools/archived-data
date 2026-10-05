use crate::{ArchiveError, MINUTES_PER_DAY, le_u32};

/// `file u8 · frame_start u32 · offset u32`.
pub(crate) const MINUTE_CELL_LEN: usize = 9;

/// `size u32 · next u32`: an L2 frame's header has no field of its own.
pub(crate) const L2_HEADER_LEN: usize = 8;

/// Every L2 frame, in a working archive and a compact one alike: a cell for every minute of the day.
pub(crate) const L2_FRAME_LEN: usize = L2_HEADER_LEN + MINUTES_PER_DAY * MINUTE_CELL_LEN;

/// The `offset` of an empty cell. A cell is written as nine `0xFF` bytes, and read as empty by this field
/// alone.
const EMPTY_OFFSET: u32 = u32::MAX;

/// Where a record starts: the data file, the frame of it — its offset in that file — and the record's
/// offset from the start of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RecordRef {
    pub file_no: u8,
    pub frame: u32,
    pub offset: u32,
}

impl RecordRef {
    pub fn new(file_no: u8, frame: u32, offset: usize) -> Self {
        Self {
            file_no,
            frame,
            offset: offset as u32,
        }
    }

    pub fn offset(&self) -> usize {
        self.offset as usize
    }
}

/// An L2 frame in memory: the cells of the 1440 minutes of one key's day, each pointing at the FIRST record
/// of its minute, or empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MinuteCells {
    cells: Vec<u8>,
}

impl MinuteCells {
    pub fn empty() -> Self {
        Self {
            cells: vec![0xFF; MINUTES_PER_DAY * MINUTE_CELL_LEN],
        }
    }

    /// The L2 frame in `bytes`, read at `offset`: exactly [`L2_FRAME_LEN`] bytes whose header states that
    /// size and no continuation. Anything else is `Corrupted`.
    pub fn decode(bytes: &[u8], offset: u64) -> Result<Self, ArchiveError> {
        let fits = bytes.len() == L2_FRAME_LEN
            && le_u32(bytes, 0) as usize == L2_FRAME_LEN
            && le_u32(bytes, 4) == 0;

        if !fits {
            return Err(ArchiveError::Corrupted(format!(
                "the L2 frame at {offset} is not {L2_FRAME_LEN} bytes with no continuation"
            )));
        }

        Ok(Self {
            cells: bytes[L2_HEADER_LEN..].to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(L2_FRAME_LEN);
        out.extend_from_slice(&(L2_FRAME_LEN as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.cells);
        out
    }

    pub fn get(&self, minute: usize) -> Option<RecordRef> {
        let at = minute * MINUTE_CELL_LEN;
        let offset = le_u32(&self.cells, at + 5);

        (offset != EMPTY_OFFSET).then(|| RecordRef {
            file_no: self.cells[at],
            frame: le_u32(&self.cells, at + 1),
            offset,
        })
    }

    pub fn set(&mut self, minute: usize, record: RecordRef) {
        debug_assert_ne!(record.offset, EMPTY_OFFSET);

        let at = minute * MINUTE_CELL_LEN;
        self.cells[at] = record.file_no;
        self.cells[at + 1..at + 5].copy_from_slice(&record.frame.to_le_bytes());
        self.cells[at + 5..at + 9].copy_from_slice(&record.offset.to_le_bytes());
    }

    /// The first minute at or after `minute` with a record.
    pub fn first_from(&self, minute: usize) -> Option<(usize, RecordRef)> {
        (minute..MINUTES_PER_DAY).find_map(|at| self.get(at).map(|record| (at, record)))
    }

    /// The last minute before `minute` with a record.
    pub fn last_before(&self, minute: usize) -> Option<(usize, RecordRef)> {
        (0..minute)
            .rev()
            .find_map(|at| self.get(at).map(|record| (at, record)))
    }

    /// Every cell with a record, rewritten by `remap`.
    pub fn remap(&mut self, mut remap: impl FnMut(RecordRef) -> RecordRef) {
        for minute in 0..MINUTES_PER_DAY {
            if let Some(record) = self.get(minute) {
                self.set(minute, remap(record));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_minute_is_nine_ff_bytes_and_is_read_by_its_offset_alone() {
        let mut cells = MinuteCells::empty();
        let bytes = cells.encode();

        assert_eq!(bytes.len(), 12_968);
        assert_eq!(&bytes[..8], &[0xA8, 0x32, 0, 0, 0, 0, 0, 0]);
        assert!(bytes[8..].iter().all(|byte| *byte == 0xFF));
        assert_eq!(cells.first_from(0), None);

        let record = RecordRef::new(3, 0x0102_0304, 0x0506);
        cells.set(1439, record);
        cells.set(7, RecordRef::new(0, 65_536, 10));

        let bytes = cells.encode();
        assert_eq!(
            &bytes[8 + 1439 * 9..],
            &[3, 4, 3, 2, 1, 6, 5, 0, 0],
            "file · frame_start · offset, little-endian"
        );

        let decoded = MinuteCells::decode(&bytes, 0).unwrap();
        assert_eq!(decoded.get(1439), Some(record));
        assert_eq!(decoded.first_from(8).map(|(minute, _)| minute), Some(1439));
        assert_eq!(decoded.last_before(1439).map(|(minute, _)| minute), Some(7));
        assert_eq!(decoded.last_before(7), None);

        // Only the offset says a cell is empty, whatever the other eight bytes hold.
        let mut other = bytes.clone();
        other[8 + 20 * 9..8 + 20 * 9 + 5].copy_from_slice(&[0; 5]);
        assert_eq!(MinuteCells::decode(&other, 0).unwrap().get(20), None);
    }

    #[test]
    fn a_frame_of_another_size_or_with_a_continuation_is_corrupted() {
        let bytes = MinuteCells::empty().encode();

        let mut chained = bytes.clone();
        chained[4] = 1;
        let mut resized = bytes.clone();
        resized[0] ^= 1;

        for (case, bytes) in [
            ("chained", chained),
            ("resized", resized),
            ("short", bytes[..100].to_vec()),
        ] {
            assert!(
                matches!(
                    MinuteCells::decode(&bytes, 0),
                    Err(ArchiveError::Corrupted(_))
                ),
                "{case}"
            );
        }
    }
}
