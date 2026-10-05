use crate::{
    ArchiveError, CheckedKeyBytes, L0_HEADER_LEN, L0FrameBody, L0FrameRead, PreallocatedDataItem,
    PreallocatedSlotKey, PrimaryKeyFixedSize, check_data_page_sizes, le_u32, le_u64,
};

/// Intervals per L0 frame of a working file.
pub const DEFAULT_L0_CAPACITY: usize = 1024;
pub const DEFAULT_L1_CAPACITY: usize = 1024;
/// The L1 capacity of every compact file.
pub const COMPACT_L1_CAPACITY: usize = 1024;

pub(crate) const L1_HEADER_LEN: usize = 8;
pub(crate) const OFFSET_LEN: usize = 8;

/// One L1 page, named by the lowest and the highest key on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct L0Entry<TKey> {
    pub from: TKey,
    pub to: TKey,
    pub offset: u64,
}

pub(crate) fn l0_entry_len<TKey: PrimaryKeyFixedSize>() -> usize {
    2 * TKey::LEN + OFFSET_LEN
}

pub(crate) fn l0_frame_len<TKey: PrimaryKeyFixedSize>(intervals: usize) -> usize {
    L0_HEADER_LEN + intervals * l0_entry_len::<TKey>()
}

pub(crate) fn l1_entry_len<TKey: PrimaryKeyFixedSize>() -> usize {
    TKey::LEN + OFFSET_LEN
}

pub(crate) fn l1_page_len<TKey: PrimaryKeyFixedSize>(capacity: usize) -> usize {
    L1_HEADER_LEN + capacity * l1_entry_len::<TKey>()
}

/// The sizes every header stores must fit their fields. A type whose constants break that can never
/// be written, so it is refused before the first byte.
pub(crate) fn check_type_sizes<TKey, TSlot, TItem>(capacity_l0: usize, capacity_l1: usize)
where
    TKey: PrimaryKeyFixedSize,
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
{
    assert!(TKey::LEN > 0, "PrimaryKeyFixedSize::LEN must be positive");
    check_data_page_sizes::<TSlot, TItem>();
    assert!(
        capacity_l0 > 0 && capacity_l1 > 0,
        "capacities must be positive"
    );
    assert!(
        u32::try_from(l1_page_len::<TKey>(capacity_l1.max(COMPACT_L1_CAPACITY))).is_ok(),
        "an L1 page must fit its u32 size"
    );
    assert!(
        u32::try_from(l0_frame_len::<TKey>(capacity_l0)).is_ok(),
        "an L0 frame must fit its u32 size"
    );
}

/// `entries` cut into frame bodies of at most `per_frame` intervals each.
pub(crate) fn encode_l0_bodies<TKey: PrimaryKeyFixedSize>(
    entries: &[L0Entry<TKey>],
    per_frame: usize,
) -> Result<Vec<L0FrameBody>, ArchiveError> {
    entries
        .chunks(per_frame)
        .map(|chunk| {
            let mut body = Vec::with_capacity(chunk.len() * l0_entry_len::<TKey>());

            for entry in chunk {
                entry.from.copy_key_checked_to(&mut body)?;
                entry.to.copy_key_checked_to(&mut body)?;
                body.extend_from_slice(&entry.offset.to_le_bytes());
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
pub(crate) fn decode_l0_chain<TKey: PrimaryKeyFixedSize>(
    frames: &[L0FrameRead],
) -> Result<Vec<L0Entry<TKey>>, ArchiveError> {
    let entry_len = l0_entry_len::<TKey>();
    let mut entries: Vec<L0Entry<TKey>> = Vec::new();

    for frame in frames {
        if frame.body.len() < frame.count * entry_len {
            return Err(ArchiveError::Corrupted(format!(
                "the L0 frame at {} declares {} bytes, too few for {} intervals",
                frame.offset,
                L0_HEADER_LEN + frame.body.len(),
                frame.count
            )));
        }

        for index in 0..frame.count {
            let at = index * entry_len;
            let bytes = &frame.body;
            let entry = L0Entry {
                from: TKey::from_slice(&bytes[at..at + TKey::LEN]),
                to: TKey::from_slice(&bytes[at + TKey::LEN..at + 2 * TKey::LEN]),
                offset: le_u64(bytes, at + 2 * TKey::LEN),
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

pub(crate) fn encode_l1_page<TKey: PrimaryKeyFixedSize>(
    entries: &[(TKey, u64)],
    capacity: usize,
) -> Result<Vec<u8>, ArchiveError> {
    if entries.len() > capacity {
        return Err(ArchiveError::ContractViolation(format!(
            "an L1 page of capacity {capacity} cannot hold {} keys",
            entries.len()
        )));
    }

    let size = l1_page_len::<TKey>(capacity);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());

    for (key, offset) in entries {
        key.copy_key_checked_to(&mut out)?;
        out.extend_from_slice(&offset.to_le_bytes());
    }

    out.resize(size, 0);
    Ok(out)
}

/// `(size, count)` of an L1 header; the capacity is taken from the page's own size.
pub(crate) fn decode_l1_header<TKey: PrimaryKeyFixedSize>(
    header: &[u8],
    offset: u64,
) -> Result<(usize, usize), ArchiveError> {
    let size = le_u32(header, 0) as usize;
    let count = le_u32(header, 4) as usize;
    let entry_len = l1_entry_len::<TKey>();

    let fits = size > L1_HEADER_LEN
        && (size - L1_HEADER_LEN).is_multiple_of(entry_len)
        && count <= (size - L1_HEADER_LEN) / entry_len
        && count > 0;

    if !fits {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 page at {offset} declares size {size} and count {count}"
        )));
    }

    Ok((size, count))
}

/// A whole page. Its keys must be strictly ascending and span exactly the L0 interval that points at
/// it, or a binary search would answer "absent" for a key that is there.
pub(crate) fn decode_l1_page<TKey: PrimaryKeyFixedSize>(
    bytes: &[u8],
    interval: &L0Entry<TKey>,
) -> Result<Vec<(TKey, u64)>, ArchiveError> {
    let offset = interval.offset;
    let (size, count) = decode_l1_header::<TKey>(bytes, offset)?;

    if bytes.len() < size {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 page at {offset} is cut short"
        )));
    }

    let entry_len = l1_entry_len::<TKey>();
    let mut entries: Vec<(TKey, u64)> = Vec::with_capacity(count);

    for index in 0..count {
        let at = L1_HEADER_LEN + index * entry_len;
        let key = TKey::from_slice(&bytes[at..at + TKey::LEN]);

        if entries.last().is_some_and(|(previous, _)| *previous >= key) {
            return Err(ArchiveError::Corrupted(format!(
                "the keys of the L1 page at {offset} are not ascending"
            )));
        }

        entries.push((key, le_u64(bytes, at + TKey::LEN)));
    }

    let spans = entries
        .first()
        .is_some_and(|(first, _)| *first == interval.from)
        && entries.last().is_some_and(|(last, _)| *last == interval.to);

    if !spans {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 page at {offset} does not span its L0 interval"
        )));
    }

    Ok(entries)
}
