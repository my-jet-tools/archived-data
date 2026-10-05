use crate::{
    ArchiveError, CheckedKeyBytes, DEFAULT_L1_CAPACITY, MICROSECONDS_PER_MINUTE,
    PrimaryKeyFixedSize, SPLIT_DATA_HEADER_LEN, SPLIT_L1_HEADER_LEN, SplitFrame, SplitFrameHeader,
    SplitL0Interval, encode_split_l1_frame, le_u32, split_l0_frame_len,
};

/// The cells of an L2 frame: one per minute of the day.
pub const MINUTES_PER_DAY: usize = 1440;

/// A record starts with its time, an `i64` little-endian of microseconds, so it is never shorter.
pub const RECORD_TIME_LEN: usize = 8;

/// The longest record an archive holds. A quarter of a data frame, so a full frame halved at the record
/// boundary nearest its middle always leaves room for the record being inserted on its side, and every
/// record fits one frame of the compact form.
pub const MAX_RECORD_LEN: usize = 8 * 1024;

/// A data frame of the working form, header included — and the most any frame of the compact form is.
pub const DATA_FRAME_LEN: usize = 32 * 1024;

/// A record's length prefix.
pub(crate) const RECORD_LEN_PREFIX: usize = 2;

/// The bytes of records a data frame of [`DATA_FRAME_LEN`] holds.
pub(crate) const DATA_FRAME_CAPACITY: usize = DATA_FRAME_LEN - SPLIT_DATA_HEADER_LEN;

// Halving a full frame at the record boundary nearest its middle leaves at most half the frame plus half a
// record on either side, and the record inserted must fit beside that: three records are at most a frame.
const _: () = assert!(3 * (RECORD_LEN_PREFIX + MAX_RECORD_LEN) <= DATA_FRAME_CAPACITY);

/// An L2 frame is addressed by a `u32` offset inside the index.
pub(crate) const L2_OFFSET_LEN: usize = 4;

pub(crate) fn records_l1_entry_len<TKey: PrimaryKeyFixedSize>() -> usize {
    TKey::LEN + L2_OFFSET_LEN
}

pub(crate) fn records_l1_page_len<TKey: PrimaryKeyFixedSize>(capacity: usize) -> usize {
    SPLIT_L1_HEADER_LEN + capacity * records_l1_entry_len::<TKey>()
}

/// The sizes every header stores must fit their fields. A key type whose constants break that can never be
/// written, so it is refused before the first byte.
pub(crate) fn check_records_type_sizes<TKey: PrimaryKeyFixedSize>(
    capacity_l0: usize,
    capacity_l1: usize,
) {
    assert!(TKey::LEN > 0, "PrimaryKeyFixedSize::LEN must be positive");
    assert!(
        capacity_l0 > 0 && capacity_l1 > 0,
        "capacities must be positive"
    );
    assert!(
        u32::try_from(records_l1_page_len::<TKey>(
            capacity_l1.max(DEFAULT_L1_CAPACITY)
        ))
        .is_ok(),
        "an L1 page must fit its u32 size"
    );
    assert!(
        u32::try_from(split_l0_frame_len::<TKey>(capacity_l0)).is_ok(),
        "an L0 frame must fit its u32 size"
    );
}

/// A record the archive cannot hold is refused: shorter than its time, or longer than [`MAX_RECORD_LEN`].
pub(crate) fn check_record_len(record: &[u8]) -> Result<(), ArchiveError> {
    if !(RECORD_TIME_LEN..=MAX_RECORD_LEN).contains(&record.len()) {
        return Err(ArchiveError::ContractViolation(format!(
            "a record of {} bytes; a record holds {RECORD_TIME_LEN} to {MAX_RECORD_LEN} bytes, its time first",
            record.len()
        )));
    }

    Ok(())
}

/// The time a record carries in its first [`RECORD_TIME_LEN`] bytes.
pub(crate) fn record_time(record: &[u8]) -> i64 {
    i64::from_le_bytes(record[..RECORD_TIME_LEN].try_into().unwrap())
}

/// The minute of the day starting at `day_start` that `time` falls in, both in microseconds; `None`
/// outside that day.
pub(crate) fn minute_of_day(day_start: i64, time: i64) -> Option<usize> {
    let since = time.checked_sub(day_start).filter(|since| *since >= 0)?;
    let minute = since / MICROSECONDS_PER_MINUTE;

    (minute < MINUTES_PER_DAY as i64).then_some(minute as usize)
}

/// ONE frame (`next` 0) of `capacity` entries, zero-padded past `entries`.
pub(crate) fn encode_records_l1_page<TKey: PrimaryKeyFixedSize>(
    entries: &[(TKey, u32)],
    capacity: usize,
) -> Result<Vec<u8>, ArchiveError> {
    if entries.len() > capacity {
        return Err(ArchiveError::ContractViolation(format!(
            "an L1 page of capacity {capacity} cannot hold {} keys",
            entries.len()
        )));
    }

    let mut body = Vec::with_capacity(entries.len() * records_l1_entry_len::<TKey>());

    for (key, l2_offset) in entries {
        key.copy_key_checked_to(&mut body)?;
        body.extend_from_slice(&l2_offset.to_le_bytes());
    }

    encode_split_l1_frame(
        records_l1_page_len::<TKey>(capacity),
        0,
        entries.len(),
        &body,
    )
}

/// The `count` of one L1 frame: at least one entry, a body of whole entries, and room for `count` of them.
pub(crate) fn check_records_l1_header<TKey: PrimaryKeyFixedSize>(
    header: &SplitFrameHeader,
) -> Result<usize, ArchiveError> {
    let entry_len = records_l1_entry_len::<TKey>();
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

/// The page chained over `frames`: every key with the offset of its L2 frame. Its keys must be strictly
/// ascending across the chain and span exactly the L0 interval that points at it.
pub(crate) fn decode_records_l1_chain<TKey: PrimaryKeyFixedSize>(
    frames: &[SplitFrame],
    interval: &SplitL0Interval<TKey>,
) -> Result<Vec<(TKey, u32)>, ArchiveError> {
    let entry_len = records_l1_entry_len::<TKey>();
    let mut entries: Vec<(TKey, u32)> = Vec::new();

    for frame in frames {
        let count = check_records_l1_header::<TKey>(&frame.header)?;

        for index in 0..count {
            let at = index * entry_len;
            let key = TKey::from_slice(&frame.body[at..at + TKey::LEN]);

            if entries.last().is_some_and(|(previous, _)| *previous >= key) {
                return Err(ArchiveError::Corrupted(format!(
                    "the keys of the L1 page at {} are not ascending",
                    interval.l1_offset
                )));
            }

            entries.push((key, le_u32(&frame.body, at + TKey::LEN)));
        }
    }

    interval.check_spanned_by(
        entries.first().map(|(key, _)| key),
        entries.last().map(|(key, _)| key),
    )?;

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_items::Login;
    use crate::{MICROSECONDS_PER_DAY, SplitFrameKind};

    const DAY: i64 = 1_789_430_400_000_000;

    #[test]
    fn a_minute_is_counted_from_the_day_start_and_nothing_outside_the_day_has_one() {
        let minute = MICROSECONDS_PER_MINUTE;

        for (time, expected) in [
            (DAY, Some(0)),
            (DAY + minute - 1, Some(0)),
            (DAY + minute, Some(1)),
            (DAY + 1439 * minute, Some(1439)),
            (DAY + MICROSECONDS_PER_DAY - 1, Some(1439)),
            (DAY + MICROSECONDS_PER_DAY, None),
            (DAY - 1, None),
            (i64::MIN, None),
            (i64::MAX, None),
        ] {
            assert_eq!(minute_of_day(DAY, time), expected, "{time}");
        }

        assert_eq!(minute_of_day(i64::MIN, i64::MAX), None);
    }

    #[test]
    fn a_record_holds_its_time_and_at_most_a_quarter_of_a_frame() {
        for len in [RECORD_TIME_LEN, 300, MAX_RECORD_LEN] {
            check_record_len(&vec![0; len]).unwrap();
        }

        for len in [0, RECORD_TIME_LEN - 1, MAX_RECORD_LEN + 1] {
            assert!(matches!(
                check_record_len(&vec![0; len]),
                Err(ArchiveError::ContractViolation(_))
            ));
        }

        assert_eq!(record_time(&(-5i64).to_le_bytes()), -5);
    }

    #[test]
    fn an_l1_page_round_trips_and_refuses_what_it_cannot_be() {
        let entries = vec![(Login(7), 100u32), (Login(9), 200)];
        let bytes = encode_records_l1_page(&entries, 3).unwrap();
        assert_eq!(bytes.len(), 12 + 3 * 12);

        let frame = SplitFrame::from_bytes(bytes, 40, SplitFrameKind::L1).unwrap();
        let interval = SplitL0Interval {
            from: Login(7),
            to: Login(9),
            l1_offset: 40,
        };
        assert_eq!(
            decode_records_l1_chain(&[frame], &interval).unwrap(),
            entries
        );

        assert!(matches!(
            encode_records_l1_page(&entries, 1),
            Err(ArchiveError::ContractViolation(_))
        ));
    }
}
