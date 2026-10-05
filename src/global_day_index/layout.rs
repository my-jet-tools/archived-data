use crate::{
    ArchiveError, PrimaryKeyFixedSize, SPLIT_L0_HEADER_LEN, SPLIT_L1_HEADER_LEN, SplitFrame,
    SplitFrameHeader, SplitL0Interval, le_u16, le_u32,
};

/// The first bytes of a global day index: when it was synchronised, an `i64` little-endian of microseconds.
pub const GLOBAL_DAY_INDEX_SYNC_TIME_LEN: usize = 8;

/// Where the L0 frame of a global day index starts: right after its sync time.
pub(crate) const DAY_INDEX_L0_AT: u64 = GLOBAL_DAY_INDEX_SYNC_TIME_LEN as u64;

/// The smallest global day index there is: its sync time and an L0 frame of no interval.
pub(crate) const MIN_DAY_INDEX_LEN: u64 = DAY_INDEX_L0_AT + SPLIT_L0_HEADER_LEN as u64;

/// `data_offset u32 · blocks u16`.
const LOGIN_DAYS_LEN: usize = 6;

/// Where a login's year blocks are: `blocks` blocks from `offset` of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoginDays {
    pub offset: u32,
    pub blocks: u16,
}

pub(crate) fn day_index_l1_entry_len<TKey: PrimaryKeyFixedSize>() -> usize {
    TKey::LEN + LOGIN_DAYS_LEN
}

pub(crate) fn day_index_l1_page_len<TKey: PrimaryKeyFixedSize>(capacity: usize) -> usize {
    SPLIT_L1_HEADER_LEN + capacity * day_index_l1_entry_len::<TKey>()
}

/// The `count` of one L1 frame: at least one entry, a body of whole entries, and room for `count` of them.
pub(crate) fn check_day_index_l1_header<TKey: PrimaryKeyFixedSize>(
    header: &SplitFrameHeader,
) -> Result<usize, ArchiveError> {
    let entry_len = day_index_l1_entry_len::<TKey>();
    let body = header.size - SPLIT_L1_HEADER_LEN;
    let count = header.field as usize;

    if count == 0 || !body.is_multiple_of(entry_len) || count > body / entry_len {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 frame at {} declares size {} and count {count}",
            header.offset, header.size
        )));
    }

    Ok(count)
}

/// The page chained over `frames`: every login with where its year blocks are. Its logins must be strictly
/// ascending across the chain and span exactly the L0 interval that points at it, and every login must have
/// at least one block.
pub(crate) fn decode_day_index_l1_chain<TKey: PrimaryKeyFixedSize>(
    frames: &[SplitFrame],
    interval: &SplitL0Interval<TKey>,
) -> Result<Vec<(TKey, LoginDays)>, ArchiveError> {
    let entry_len = day_index_l1_entry_len::<TKey>();
    let mut entries: Vec<(TKey, LoginDays)> = Vec::new();

    for frame in frames {
        let count = check_day_index_l1_header::<TKey>(&frame.header)?;

        for index in 0..count {
            let at = index * entry_len;
            let login = TKey::from_slice(&frame.body[at..at + TKey::LEN]);
            let days = LoginDays {
                offset: le_u32(&frame.body, at + TKey::LEN),
                blocks: le_u16(&frame.body, at + TKey::LEN + 4),
            };

            if entries
                .last()
                .is_some_and(|(previous, _)| *previous >= login)
                || days.blocks == 0
            {
                return Err(ArchiveError::Corrupted(format!(
                    "the L1 page at {} holds logins out of order, or a login with no year",
                    interval.l1_offset
                )));
            }

            entries.push((login, days));
        }
    }

    interval.check_spanned_by(
        entries.first().map(|(login, _)| login),
        entries.last().map(|(login, _)| login),
    )?;

    Ok(entries)
}
