use crate::{
    ArchiveError, RECORD_LEN_PREFIX, RECORD_TIME_LEN, SPLIT_DATA_HEADER_LEN, SplitFrameHeader,
    SplitFrameKind, le_u16, le_u32, record_time,
};

/// The record starting at `at` of a data frame read at `frame_at`, whose records end at `end`: its length
/// with the prefix, and its time. A record that does not fit before `end`, or is shorter than its time, is
/// `Corrupted`.
pub(crate) fn frame_record(
    frame: &[u8],
    at: usize,
    end: usize,
    frame_at: u32,
) -> Result<(usize, i64), ArchiveError> {
    let corrupted = || {
        ArchiveError::Corrupted(format!(
            "the data frame at {frame_at} holds no whole record at {at}; its records end at {end}"
        ))
    };

    if at + RECORD_LEN_PREFIX > end {
        return Err(corrupted());
    }

    let len = le_u16(frame, at) as usize;
    let whole = RECORD_LEN_PREFIX + len;

    if len < RECORD_TIME_LEN || at + whole > end {
        return Err(corrupted());
    }

    Ok((whole, record_time(&frame[at + RECORD_LEN_PREFIX..])))
}

/// The header of the data frame at the start of `bytes`, read at `offset`: `(size, next, used)`. A header
/// that does not decode, or whose records reach past its size, is `Corrupted`.
pub(crate) fn decode_records_frame_header(
    bytes: &[u8],
    offset: u32,
) -> Result<(usize, u32, usize), ArchiveError> {
    let header = SplitFrameHeader::decode(bytes, offset as u64, SplitFrameKind::Data)?;
    let used = header.field as usize;

    if SPLIT_DATA_HEADER_LEN + used > header.size {
        return Err(ArchiveError::Corrupted(format!(
            "the data frame at {offset} declares {} bytes and {used} of records",
            header.size
        )));
    }

    Ok((header.size, header.next, used))
}

/// A data frame of a WORKING archive in memory: exactly its size in bytes — the header, the records, and
/// the free space after them, kept zeroed.
pub(crate) struct RecordsFrame {
    bytes: Vec<u8>,
}

impl RecordsFrame {
    pub fn new(size: usize) -> Self {
        let mut bytes = vec![0; size];
        bytes[..4].copy_from_slice(&(size as u32).to_le_bytes());
        Self { bytes }
    }

    /// The frame read back from `offset`, which must state exactly `bytes.len()` bytes.
    pub fn decode(bytes: Vec<u8>, offset: u32) -> Result<Self, ArchiveError> {
        let (size, _, _) = decode_records_frame_header(&bytes, offset)?;

        if size != bytes.len() {
            return Err(ArchiveError::Corrupted(format!(
                "the data frame at {offset} declares {size} bytes; the working form's frames are {}",
                bytes.len()
            )));
        }

        Ok(Self { bytes })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn next(&self) -> u32 {
        le_u32(&self.bytes, 4)
    }

    pub fn set_next(&mut self, next: u32) {
        self.bytes[4..8].copy_from_slice(&next.to_le_bytes());
    }

    /// Where the records end.
    pub fn end(&self) -> usize {
        SPLIT_DATA_HEADER_LEN + le_u16(&self.bytes, 8) as usize
    }

    pub fn free(&self) -> usize {
        self.bytes.len() - self.end()
    }

    pub fn record_at(&self, at: usize, frame_at: u32) -> Result<(usize, i64), ArchiveError> {
        frame_record(&self.bytes, at, self.end(), frame_at)
    }

    /// The record boundary nearest the middle of the records, the frame's first and last edge excluded; ties
    /// go to the lower one. `None` when the frame holds fewer than two records.
    pub fn halving_boundary(&self, frame_at: u32) -> Result<Option<usize>, ArchiveError> {
        let (end, used) = (self.end(), self.end() - SPLIT_DATA_HEADER_LEN);
        let distance = |boundary: usize| (2 * (boundary - SPLIT_DATA_HEADER_LEN)).abs_diff(used);

        let mut best: Option<usize> = None;
        let mut at = SPLIT_DATA_HEADER_LEN;

        while at < end {
            at += self.record_at(at, frame_at)?.0;

            if at < end && best.is_none_or(|best| distance(at) < distance(best)) {
                best = Some(at);
            }
        }

        Ok(best)
    }

    /// Writes `bytes` at `at`, moving the records from `at` on after them. The caller made sure they fit.
    pub fn insert(&mut self, at: usize, bytes: &[u8]) {
        let end = self.end();
        assert!(
            at >= SPLIT_DATA_HEADER_LEN && at <= end && bytes.len() <= self.free(),
            "{} bytes inserted at {at} of a frame whose records end at {end} with {} free",
            bytes.len(),
            self.free()
        );

        self.bytes.copy_within(at..end, at + bytes.len());
        self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
        self.set_end(end + bytes.len());
    }

    /// Takes the records from `at` on out of the frame and zeroes the room they leave.
    pub fn split_off(&mut self, at: usize) -> Vec<u8> {
        let end = self.end();
        let tail = self.bytes[at..end].to_vec();

        self.bytes[at..end].fill(0);
        self.set_end(at);
        tail
    }

    fn set_end(&mut self, end: usize) {
        let used = (end - SPLIT_DATA_HEADER_LEN) as u16;
        self.bytes[8..10].copy_from_slice(&used.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SPLIT_DATA_HEADER_LEN as HEADER;

    /// A record of `len` bytes with time `time`, prefixed.
    fn prefixed(time: i64, len: usize) -> Vec<u8> {
        let mut out = (len as u16).to_le_bytes().to_vec();
        out.extend_from_slice(&time.to_le_bytes());
        out.resize(RECORD_LEN_PREFIX + len, time as u8);
        out
    }

    #[test]
    fn records_are_inserted_before_others_and_split_off_with_the_room_zeroed() {
        let mut frame = RecordsFrame::new(100);
        assert_eq!((frame.end(), frame.free(), frame.next()), (HEADER, 90, 0));

        frame.insert(HEADER, &prefixed(30, 10));
        frame.insert(HEADER, &prefixed(10, 20));
        let at = HEADER + 22;
        frame.insert(at, &prefixed(20, 8));
        assert_eq!(frame.end(), HEADER + 22 + 10 + 12);

        let times: Vec<i64> = [HEADER, HEADER + 22, HEADER + 32]
            .into_iter()
            .map(|at| frame.record_at(at, 0).unwrap().1)
            .collect();
        assert_eq!(times, [10, 20, 30]);

        // Boundaries at 22 and 32 of 44 used: 22 is the middle exactly.
        assert_eq!(frame.halving_boundary(0).unwrap(), Some(HEADER + 22));

        let tail = frame.split_off(HEADER + 22);
        assert_eq!(tail, [prefixed(20, 8), prefixed(30, 10)].concat());
        assert_eq!(frame.end(), HEADER + 22);
        assert!(frame.bytes()[HEADER + 22..].iter().all(|byte| *byte == 0));

        frame.set_next(4096);
        let decoded = RecordsFrame::decode(frame.bytes().to_vec(), 7).unwrap();
        assert_eq!((decoded.next(), decoded.end()), (4096, HEADER + 22));
        assert_eq!(decoded.halving_boundary(0).unwrap(), None);
    }

    #[test]
    fn a_record_past_the_end_of_the_records_or_shorter_than_its_time_is_corrupted() {
        let mut frame = RecordsFrame::new(64);
        frame.insert(HEADER, &prefixed(1, 8));

        assert!(frame.record_at(HEADER + 10, 0).is_err());

        let mut bytes = frame.bytes().to_vec();
        bytes[HEADER..HEADER + 2].copy_from_slice(&7u16.to_le_bytes());
        assert!(matches!(
            frame_record(&bytes, HEADER, HEADER + 10, 0),
            Err(ArchiveError::Corrupted(_))
        ));

        bytes[HEADER..HEADER + 2].copy_from_slice(&9u16.to_le_bytes());
        assert!(frame_record(&bytes, HEADER, HEADER + 10, 0).is_err());

        let mut overfull = frame.bytes().to_vec();
        overfull[8..10].copy_from_slice(&60u16.to_le_bytes());
        assert!(RecordsFrame::decode(overfull, 0).is_err());
        assert!(RecordsFrame::decode(frame.bytes()[..60].to_vec(), 0).is_err());
    }
}
