use crate::{
    ArchiveError, COMPACT_L1_CAPACITY, CheckedKeyBytes, DATA_REF_LEN, DataRef, L0FrameBody,
    PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyFixedSize, SPLIT_L0_HEADER_LEN,
    SPLIT_L1_HEADER_LEN, SplitFrame, SplitFrameHeader, SplitL0Interval,
    check_split_data_page_sizes, encode_split_l1_frame, le_u32,
};

/// An L1 page is addressed by a `u32` offset inside the index.
pub(crate) const SPLIT_L1_OFFSET_LEN: usize = 4;

pub(crate) fn split_l0_entry_len<TKey: PrimaryKeyFixedSize>() -> usize {
    2 * TKey::LEN + SPLIT_L1_OFFSET_LEN
}

pub(crate) fn split_l0_frame_len<TKey: PrimaryKeyFixedSize>(intervals: usize) -> usize {
    SPLIT_L0_HEADER_LEN + intervals * split_l0_entry_len::<TKey>()
}

pub(crate) fn split_l1_entry_len<TKey: PrimaryKeyFixedSize>() -> usize {
    TKey::LEN + DATA_REF_LEN
}

pub(crate) fn split_l1_page_len<TKey: PrimaryKeyFixedSize>(capacity: usize) -> usize {
    SPLIT_L1_HEADER_LEN + capacity * split_l1_entry_len::<TKey>()
}

/// The sizes every split header stores must fit their fields. A type whose constants break that can never
/// be written, so it is refused before the first byte.
pub(crate) fn check_split_type_sizes<TKey, TSlot, TItem>(capacity_l0: usize, capacity_l1: usize)
where
    TKey: PrimaryKeyFixedSize,
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
{
    assert!(TKey::LEN > 0, "PrimaryKeyFixedSize::LEN must be positive");
    check_split_data_page_sizes::<TSlot, TItem>();
    assert!(
        capacity_l0 > 0 && capacity_l1 > 0,
        "capacities must be positive"
    );
    assert!(
        u32::try_from(split_l1_page_len::<TKey>(
            capacity_l1.max(COMPACT_L1_CAPACITY)
        ))
        .is_ok(),
        "an L1 page must fit its u32 size"
    );
    assert!(
        u32::try_from(split_l0_frame_len::<TKey>(capacity_l0)).is_ok(),
        "an L0 frame must fit its u32 size"
    );
}

/// `entries` cut into frame bodies of at most `per_frame` intervals each.
pub(crate) fn encode_split_l0_bodies<TKey: PrimaryKeyFixedSize>(
    entries: &[SplitL0Interval<TKey>],
    per_frame: usize,
) -> Result<Vec<L0FrameBody>, ArchiveError> {
    entries
        .chunks(per_frame)
        .map(|chunk| {
            let mut body = Vec::with_capacity(chunk.len() * split_l0_entry_len::<TKey>());

            for entry in chunk {
                entry.from.copy_key_checked_to(&mut body)?;
                entry.to.copy_key_checked_to(&mut body)?;
                body.extend_from_slice(&entry.l1_offset.to_le_bytes());
            }

            Ok(L0FrameBody {
                count: chunk.len(),
                body,
            })
        })
        .collect()
}

/// The intervals of the whole chain, in order. They must be ordered and disjoint ACROSS frames: the
/// lookup is one binary search over all of them.
pub(crate) fn decode_split_l0_chain<TKey: PrimaryKeyFixedSize>(
    frames: &[SplitFrame],
) -> Result<Vec<SplitL0Interval<TKey>>, ArchiveError> {
    let entry_len = split_l0_entry_len::<TKey>();
    let mut entries: Vec<SplitL0Interval<TKey>> = Vec::new();

    for frame in frames {
        let count = frame.header.field as usize;

        if count
            .checked_mul(entry_len)
            .is_none_or(|needed| needed > frame.body.len())
        {
            return Err(ArchiveError::Corrupted(format!(
                "the L0 frame at {} declares {} bytes, too few for {count} intervals",
                frame.header.offset, frame.header.size
            )));
        }

        for index in 0..count {
            let at = index * entry_len;
            let bytes = &frame.body;
            let entry = SplitL0Interval {
                from: TKey::from_slice(&bytes[at..at + TKey::LEN]),
                to: TKey::from_slice(&bytes[at + TKey::LEN..at + 2 * TKey::LEN]),
                l1_offset: le_u32(bytes, at + 2 * TKey::LEN),
            };

            let ordered = entry.from <= entry.to
                && entries
                    .last()
                    .is_none_or(|previous| previous.to < entry.from);

            if !ordered {
                return Err(ArchiveError::Corrupted(format!(
                    "L0 interval {} is not ordered after the previous one",
                    entries.len()
                )));
            }

            entries.push(entry);
        }
    }

    Ok(entries)
}

/// ONE frame (`next` 0) of `capacity` entries, zero-padded past `entries`.
pub(crate) fn encode_split_l1_page<TKey: PrimaryKeyFixedSize>(
    entries: &[(TKey, DataRef)],
    capacity: usize,
) -> Result<Vec<u8>, ArchiveError> {
    if entries.len() > capacity {
        return Err(ArchiveError::ContractViolation(format!(
            "an L1 page of capacity {capacity} cannot hold {} keys",
            entries.len()
        )));
    }

    let mut body = Vec::with_capacity(entries.len() * split_l1_entry_len::<TKey>());

    for (key, data) in entries {
        key.copy_key_checked_to(&mut body)?;
        data.encode_to(&mut body);
    }

    encode_split_l1_frame(split_l1_page_len::<TKey>(capacity), 0, entries.len(), &body)
}

/// The `count` of one L1 frame. Entries never straddle frames, so every frame is checked on its own: at
/// least one entry, a body of whole entries, and room for `count` of them.
pub(crate) fn check_split_l1_header<TKey: PrimaryKeyFixedSize>(
    header: &SplitFrameHeader,
) -> Result<usize, ArchiveError> {
    let entry_len = split_l1_entry_len::<TKey>();
    let body = header.size - SPLIT_L1_HEADER_LEN;
    let count = header.field as usize;

    let fits = count > 0 && body.is_multiple_of(entry_len) && count <= body / entry_len;

    if !fits {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 frame at {} declares size {} and count {count}",
            header.offset, header.size
        )));
    }

    Ok(count)
}

/// The page chained over `frames`. Its keys must be strictly ascending across the chain and span exactly
/// the L0 interval that points at it, or a binary search would answer "absent" for a key that is there.
pub(crate) fn decode_split_l1_chain<TKey: PrimaryKeyFixedSize>(
    frames: &[SplitFrame],
    interval: &SplitL0Interval<TKey>,
) -> Result<Vec<(TKey, DataRef)>, ArchiveError> {
    let offset = interval.l1_offset;
    let entry_len = split_l1_entry_len::<TKey>();
    let mut entries: Vec<(TKey, DataRef)> = Vec::new();

    for frame in frames {
        let count = check_split_l1_header::<TKey>(&frame.header)?;

        for index in 0..count {
            let at = index * entry_len;
            let key = TKey::from_slice(&frame.body[at..at + TKey::LEN]);

            if entries.last().is_some_and(|(previous, _)| *previous >= key) {
                return Err(ArchiveError::Corrupted(format!(
                    "the keys of the L1 page at {offset} are not ascending"
                )));
            }

            entries.push((key, DataRef::decode(&frame.body, at + TKey::LEN)));
        }
    }

    interval.check_spanned_by(
        entries.first().map(|(key, _)| key),
        entries.last().map(|(key, _)| key),
    )?;

    Ok(entries)
}
