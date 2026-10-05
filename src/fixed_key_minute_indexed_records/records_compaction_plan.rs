use crate::{
    ArchiveError, COMPACT_L1_CAPACITY, CompactRecordsRun, L2_FRAME_LEN, L2_OFFSET_LEN,
    MAX_DATA_FILES, MAX_INDEX_BYTES, SPLIT_L0_HEADER_LEN, SPLIT_L1_HEADER_LEN,
};

/// One key of a compaction: its bytes as the index stores them, and where its run of frames lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedRecordsKey {
    pub key: Vec<u8>,
    pub file_no: u8,
    pub run_start: u32,
    pub run: CompactRecordsRun,
}

/// What a compaction of a day archive of records will write, worked out before a byte of it is written:
/// where every key's records land, and the exact size of the compact index and of every compact data file.
///
/// A caller whose writers must know a file's size up front (an S3 upload) builds its factory from
/// [`Self::index_size`] and [`Self::data_file_sizes`], then hands the plan back to `write_compacted_to`,
/// which refuses a source that no longer matches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinuteIndexedRecordsCompactionPlan {
    pub(crate) keys: Vec<PlannedRecordsKey>,
    pub(crate) key_len: usize,
    pub(crate) data_file_sizes: Vec<u64>,
    records: u64,
    l0_len: u64,
    index_size: u64,
}

impl MinuteIndexedRecordsCompactionPlan {
    /// The plan of `keys`, in key order, whose runs already have their data files. An index past 4 GiB is a
    /// `ContractViolation`.
    pub(crate) fn new(
        keys: Vec<PlannedRecordsKey>,
        key_len: usize,
        data_file_sizes: Vec<u64>,
    ) -> Result<Self, ArchiveError> {
        let pages = keys.len().div_ceil(COMPACT_L1_CAPACITY) as u64;
        let l0_len = SPLIT_L0_HEADER_LEN as u64 + pages * (2 * key_len + L2_OFFSET_LEN) as u64;

        let index_size = l0_len
            + keys.len() as u64 * L2_FRAME_LEN as u64
            + pages * compact_l1_page_len(key_len) as u64;

        if index_size > MAX_INDEX_BYTES {
            return Err(ArchiveError::ContractViolation(format!(
                "a compact index of {} keys would be {index_size} bytes, past {MAX_INDEX_BYTES}",
                keys.len()
            )));
        }

        let records = keys.iter().map(|key| key.run.records).sum();

        Ok(Self {
            keys,
            key_len,
            data_file_sizes,
            records,
            l0_len,
            index_size,
        })
    }

    pub fn index_size(&self) -> u64 {
        self.index_size
    }

    /// One size per data file, in file order; none for an archive with no records.
    pub fn data_file_sizes(&self) -> &[u64] {
        &self.data_file_sizes
    }

    pub fn keys_count(&self) -> usize {
        self.keys.len()
    }

    pub fn records_count(&self) -> u64 {
        self.records
    }

    pub(crate) fn l0_len(&self) -> u64 {
        self.l0_len
    }

    /// Where the L2 frame of the key of `rank` starts: the frames follow L0, one per key in key order.
    pub(crate) fn l2_offset(&self, rank: usize) -> u32 {
        (self.l0_len + rank as u64 * L2_FRAME_LEN as u64) as u32
    }

    /// Where the L1 page of `page` starts: the pages follow the last L2 frame.
    pub(crate) fn l1_offset(&self, page: usize) -> u32 {
        let first = self.l0_len + self.keys.len() as u64 * L2_FRAME_LEN as u64;
        (first + page as u64 * compact_l1_page_len(self.key_len) as u64) as u32
    }
}

pub(crate) fn compact_l1_page_len(key_len: usize) -> usize {
    SPLIT_L1_HEADER_LEN + COMPACT_L1_CAPACITY * (key_len + L2_OFFSET_LEN)
}

/// Where a run of `len` bytes lands: in the last data file of `files` when it fits there whole under
/// `max_data_file_bytes`, otherwise at the start of the next one — so a key's records never cross data files,
/// and a data file ends only between keys. A run with no records lands nowhere. A run longer than a data
/// file, or needing data file 256, is a `ContractViolation`.
pub(crate) fn place_records_run(
    files: &mut Vec<u64>,
    len: u64,
    max_data_file_bytes: u64,
) -> Result<(u8, u32), ArchiveError> {
    if len == 0 {
        return Ok((0, 0));
    }

    if len > max_data_file_bytes {
        return Err(ArchiveError::ContractViolation(format!(
            "the records of one key take {len} bytes, more than a data file of {max_data_file_bytes} holds"
        )));
    }

    let last = files.len().checked_sub(1);

    if let Some(last) = last.filter(|last| files[*last] + len <= max_data_file_bytes) {
        let start = files[last];
        files[last] += len;
        return Ok((last as u8, start as u32));
    }

    if files.len() == MAX_DATA_FILES {
        return Err(ArchiveError::ContractViolation(format!(
            "the records of a key need data file {MAX_DATA_FILES}; there are {MAX_DATA_FILES}"
        )));
    }

    files.push(len);
    Ok(((files.len() - 1) as u8, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_goes_into_the_last_file_only_whole() {
        let mut files = Vec::new();

        let placed: Vec<(u8, u32)> = [40, 50, 0, 20, 100, 1]
            .into_iter()
            .map(|len| place_records_run(&mut files, len, 100).unwrap())
            .collect();

        assert_eq!(placed, [(0, 0), (0, 40), (0, 0), (1, 0), (2, 0), (3, 0)]);
        assert_eq!(files, [90, 20, 100, 1]);

        assert!(matches!(
            place_records_run(&mut files, 101, 100),
            Err(ArchiveError::ContractViolation(_))
        ));

        let mut full = vec![100; MAX_DATA_FILES];
        assert!(place_records_run(&mut full, 1, 100).is_err());
        assert_eq!(full.len(), MAX_DATA_FILES);
    }

    #[test]
    fn the_index_is_l0_then_an_l2_frame_per_key_then_the_l1_pages() {
        let keys = (0..1025u64)
            .map(|rank| PlannedRecordsKey {
                key: rank.to_le_bytes().to_vec(),
                file_no: 0,
                run_start: 0,
                run: CompactRecordsRun {
                    len: 0,
                    records: 2,
                    cells: Vec::new(),
                },
            })
            .collect();

        let plan = MinuteIndexedRecordsCompactionPlan::new(keys, 8, Vec::new()).unwrap();
        let l0 = 20 + 2 * 20;
        let page = 12 + 1024 * 12;

        assert_eq!(plan.l0_len(), l0);
        assert_eq!(plan.l2_offset(1), (l0 + 12_968) as u32);
        assert_eq!(plan.l1_offset(1), (l0 + 1025 * 12_968 + page) as u32);
        assert_eq!(plan.index_size(), l0 + 1025 * 12_968 + 2 * page);
        assert_eq!((plan.keys_count(), plan.records_count()), (1025, 2050));

        let empty = MinuteIndexedRecordsCompactionPlan::new(Vec::new(), 8, Vec::new()).unwrap();
        assert_eq!((empty.index_size(), empty.data_file_sizes()), (20, &[][..]));
    }
}
