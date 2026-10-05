use std::collections::BTreeMap;

use rust_extensions::date_time::DateTimeAsMicroseconds;

use crate::{
    ArchiveError, ArchiveFileReader, BytesFileReader, COMPACT_L1_CAPACITY, CheckedKeyBytes,
    DAY_INDEX_L0_AT, GLOBAL_DAY_INDEX_SYNC_TIME_LEN, GlobalDayIndexReader, PrimaryKeyFixedSize,
    YEAR_BLOCK_LEN, YearDays, day_index_l1_page_len, days_between, encode_split_l0_frame,
    encode_split_l1_frame, split_l0_frame_len, sync_time_of,
};

/// A global day index in memory: for every login, the days it has data on, by year. What an archiver
/// builds a new version from — the old one, plus the logins of the day it archives — and writes out whole.
///
/// ```text
/// sync time   i64 LE of microseconds                                    8 bytes
/// L0          ONE split L0 frame, next 0                                size u32 · next u32 · count u32 · hash u64, then count × (from K · to K · l1_offset u32)
/// L1          full pages of 1024 logins, the last zero-padded           size u32 · next u32 · count u32, then count × (login K · data_offset u32 · blocks u16)
/// data        every login's year blocks, in login order                 blocks × (year u16 · 46 bytes of days)
/// ```
///
/// The bytes are a function of the sync time and the days alone. Setting a day is idempotent, so building a
/// version again after a failure changes nothing a first build did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalDayIndex<TKey> {
    sync_time: DateTimeAsMicroseconds,
    logins: BTreeMap<TKey, Vec<YearDays>>,
}

impl<TKey: PrimaryKeyFixedSize> GlobalDayIndex<TKey> {
    /// An index of no login.
    pub fn new(sync_time: DateTimeAsMicroseconds) -> Self {
        Self {
            sync_time,
            logins: BTreeMap::new(),
        }
    }

    /// The index held in `bytes`, read and checked exactly as [`GlobalDayIndexReader`] reads a file.
    pub async fn from_bytes(bytes: Vec<u8>) -> Result<Self, ArchiveError> {
        Self::read_from(BytesFileReader::new(bytes)).await
    }

    /// The whole index `reader` reads: every L1 page and every login's year blocks, each checked. A file that
    /// is not a global day index, or is damaged anywhere, is `Corrupted`.
    pub async fn read_from<TReader: ArchiveFileReader>(
        reader: TReader,
    ) -> Result<Self, ArchiveError> {
        let reader = GlobalDayIndexReader::<TKey, TReader>::open(reader).await?;
        let mut logins = BTreeMap::new();

        for interval in reader.l0() {
            for (login, days) in reader.source_l1_page(interval).await? {
                let blocks = reader.year_blocks(days).await?;

                if logins.insert(login, blocks).is_some() {
                    return Err(ArchiveError::Corrupted(format!(
                        "the L1 page at {} repeats a login of another page",
                        interval.l1_offset
                    )));
                }
            }
        }

        Ok(Self {
            sync_time: reader.sync_time(),
            logins,
        })
    }

    /// The sync time the first [`GLOBAL_DAY_INDEX_SYNC_TIME_LEN`] bytes of an index state — what one ranged
    /// read of an S3 object returns. Fewer bytes is `Corrupted`.
    pub fn sync_time_of(first_bytes: &[u8]) -> Result<DateTimeAsMicroseconds, ArchiveError> {
        if first_bytes.len() < GLOBAL_DAY_INDEX_SYNC_TIME_LEN {
            return Err(ArchiveError::Corrupted(format!(
                "{} bytes of a global day index; its sync time is {GLOBAL_DAY_INDEX_SYNC_TIME_LEN}",
                first_bytes.len()
            )));
        }

        Ok(sync_time_of(first_bytes))
    }

    pub fn sync_time(&self) -> DateTimeAsMicroseconds {
        self.sync_time
    }

    pub fn set_sync_time(&mut self, sync_time: DateTimeAsMicroseconds) {
        self.sync_time = sync_time;
    }

    pub fn logins_count(&self) -> usize {
        self.logins.len()
    }

    /// Marks the day `day` falls in as a day `login` has data on: a login not in the index is added, a year
    /// it has no block for gets one, and a day already set stays as it is. A day in a year a `u16` cannot hold
    /// is refused and changes nothing.
    pub fn set_day(
        &mut self,
        login: TKey,
        day: DateTimeAsMicroseconds,
    ) -> Result<(), ArchiveError> {
        let Some((year, position)) = YearDays::day_of(day.unix_microseconds) else {
            return Err(ArchiveError::ContractViolation(format!(
                "the day of {} µs is in a year a year block cannot hold",
                day.unix_microseconds
            )));
        };

        let blocks = self.logins.entry(login).or_default();

        let at = match blocks.binary_search_by_key(&year, |block| block.year) {
            Ok(at) => at,
            Err(at) => {
                blocks.insert(at, YearDays::new(year));
                at
            }
        };

        blocks[at].set(position);
        Ok(())
    }

    /// Every day `login` has data on from the day of `from` to the day of `to`, both included, ascending, each
    /// as its first microsecond. A login the index does not hold, and `from > to`, is an empty list.
    pub fn days_with_data(
        &self,
        login: &TKey,
        from: DateTimeAsMicroseconds,
        to: DateTimeAsMicroseconds,
    ) -> Vec<DateTimeAsMicroseconds> {
        match self.logins.get(login) {
            Some(blocks) => days_between(blocks, from, to),
            None => Vec::new(),
        }
    }

    /// The whole file. A file past 4 GiB — its offsets are `u32` — a login with more than 65 535 years, or a
    /// key whose `copy_to` breaks its contract is a `ContractViolation`.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ArchiveError> {
        let pages: Vec<Vec<(&TKey, &Vec<YearDays>)>> = self
            .logins
            .iter()
            .collect::<Vec<_>>()
            .chunks(COMPACT_L1_CAPACITY)
            .map(<[_]>::to_vec)
            .collect();

        let l0_len = split_l0_frame_len::<TKey>(pages.len());
        let page_len = day_index_l1_page_len::<TKey>(COMPACT_L1_CAPACITY);
        let first_page = DAY_INDEX_L0_AT as usize + l0_len;
        let data_start = first_page + pages.len() * page_len;

        let blocks: usize = self.logins.values().map(Vec::len).sum();
        let total = data_start as u64 + (blocks * YEAR_BLOCK_LEN) as u64;

        if total > u32::MAX as u64 + 1 {
            return Err(ArchiveError::ContractViolation(format!(
                "a global day index of {total} bytes; its offsets are u32"
            )));
        }

        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&self.sync_time.unix_microseconds.to_le_bytes());

        let mut l0 = Vec::with_capacity(l0_len);

        for (page, logins) in pages.iter().enumerate() {
            logins[0].0.copy_key_checked_to(&mut l0)?;
            logins[logins.len() - 1].0.copy_key_checked_to(&mut l0)?;
            l0.extend_from_slice(&((first_page + page * page_len) as u32).to_le_bytes());
        }

        out.extend_from_slice(&encode_split_l0_frame(l0_len, 0, pages.len(), &l0)?);

        let mut data_offset = data_start;

        for logins in &pages {
            let mut body = Vec::with_capacity(page_len);

            for (login, blocks) in logins {
                let Ok(count) = u16::try_from(blocks.len()) else {
                    return Err(ArchiveError::ContractViolation(format!(
                        "a login with {} years; a count of blocks is u16",
                        blocks.len()
                    )));
                };

                login.copy_key_checked_to(&mut body)?;
                body.extend_from_slice(&(data_offset as u32).to_le_bytes());
                body.extend_from_slice(&count.to_le_bytes());
                data_offset += blocks.len() * YEAR_BLOCK_LEN;
            }

            out.extend_from_slice(&encode_split_l1_frame(page_len, 0, logins.len(), &body)?);
        }

        for blocks in self.logins.values() {
            for block in blocks {
                block.encode_to(&mut out);
            }
        }

        debug_assert_eq!(out.len() as u64, total);
        Ok(out)
    }
}
