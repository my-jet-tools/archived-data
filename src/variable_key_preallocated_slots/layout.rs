use crate::{
    ArchiveError, EntryWalk, L0_HEADER_LEN, L0FrameBody, L0FrameRead, MAX_VARIABLE_KEY_LEN,
    PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyVariableSize, check_data_page_sizes,
    count_mismatch, encode_key_prefix, key_prefix_len, le_u32,
};

/// Bytes per L0 frame of a working file.
pub const DEFAULT_L0_FRAME_BYTES: usize = 64 * 1024;
/// Bytes per L1 page of a working file.
pub const DEFAULT_L1_PAGE_BYTES: usize = 16 * 1024;
/// A compact L1 page holds at most this many keys…
pub const COMPACT_L1_MAX_KEYS: usize = 1024;
/// …and closes before an entry that would take it past this many bytes, header included.
pub const COMPACT_L1_PAGE_BYTES: usize = 16 * 1024;

pub(crate) const VARIABLE_L1_HEADER_LEN: usize = 8;
const OFFSET_BYTES: usize = 8;

/// An L1 page, named by the lowest and the highest key on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariableL0Interval<TKey> {
    pub from: TKey,
    pub to: TKey,
    pub offset: u64,
}

/// One entry of an L1 page as read: the key, where its raw bytes are inside the page, its data page.
pub(crate) struct VariableL1Entry<TKey> {
    pub key: TKey,
    pub key_at: usize,
    pub key_len: usize,
    pub offset: u64,
}

/// prefix + key + offset.
pub(crate) fn variable_l1_entry_len(key_len: usize) -> usize {
    key_prefix_len(key_len) + key_len + OFFSET_BYTES
}

/// prefix + from + prefix + to + offset.
pub(crate) fn variable_l0_entry_len(from_len: usize, to_len: usize) -> usize {
    key_prefix_len(from_len) + from_len + key_prefix_len(to_len) + to_len + OFFSET_BYTES
}

/// `2 × entry ≤ l1_page_bytes − VARIABLE_L1_HEADER_LEN`. At most half the room of a page is what makes
/// a split of an overfull page always produce exactly two pages that fit.
pub(crate) fn max_variable_l1_entry_len(l1_page_bytes: usize) -> usize {
    (l1_page_bytes - VARIABLE_L1_HEADER_LEN) / 2
}

pub(crate) fn refuse_oversized_key(
    key_len: usize,
    l1_page_bytes: usize,
) -> Result<(), ArchiveError> {
    let entry = variable_l1_entry_len(key_len);
    let max = max_variable_l1_entry_len(l1_page_bytes);

    if entry > max {
        return Err(ArchiveError::ContractViolation(format!(
            "key too large: {key_len} bytes make an L1 entry of {entry} bytes, and an L1 page of \
             {l1_page_bytes} bytes accepts at most {max}"
        )));
    }

    Ok(())
}

/// Sizes that can never be written are refused before the first byte.
pub(crate) fn check_variable_sizes<TSlot, TItem>(l0_frame_bytes: usize, l1_page_bytes: usize)
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
{
    check_data_page_sizes::<TSlot, TItem>();
    assert!(
        l1_page_bytes >= VARIABLE_L1_HEADER_LEN + 2 * variable_l1_entry_len(1),
        "an L1 page must hold two entries of a one-byte key"
    );
    assert!(
        u32::try_from(l1_page_bytes).is_ok() && u32::try_from(l0_frame_bytes).is_ok(),
        "an L0 frame and an L1 page must fit their u32 size"
    );

    let largest_key = (max_variable_l1_entry_len(l1_page_bytes) - OFFSET_BYTES)
        .min(key_prefix_len(MAX_VARIABLE_KEY_LEN) + MAX_VARIABLE_KEY_LEN);
    assert!(
        l0_frame_bytes >= L0_HEADER_LEN + 2 * largest_key + OFFSET_BYTES,
        "an L0 frame must hold one interval of the largest key its L1 pages accept"
    );
}

pub(crate) fn push_key_field(out: &mut Vec<u8>, key: &[u8]) {
    encode_key_prefix(key.len(), out);
    out.extend_from_slice(key);
}

/// `(from, to, offset)` packed greedily into frame bodies of at most `body_capacity` bytes. Every entry
/// fits a body on its own — [`check_variable_sizes`] and [`refuse_oversized_key`] see to it.
pub(crate) fn encode_variable_l0_bodies<'a>(
    entries: impl Iterator<Item = (&'a [u8], &'a [u8], u64)>,
    body_capacity: usize,
) -> Vec<L0FrameBody> {
    let mut bodies: Vec<L0FrameBody> = Vec::new();

    for (from, to, offset) in entries {
        let entry_len = variable_l0_entry_len(from.len(), to.len());

        let full = bodies
            .last()
            .is_none_or(|body| body.body.len() + entry_len > body_capacity);

        if full {
            bodies.push(L0FrameBody {
                count: 0,
                body: Vec::new(),
            });
        }

        let body = bodies.last_mut().unwrap();
        push_key_field(&mut body.body, from);
        push_key_field(&mut body.body, to);
        body.body.extend_from_slice(&offset.to_le_bytes());
        body.count += 1;
    }

    bodies
}

pub(crate) fn begin_variable_l1_page() -> Vec<u8> {
    vec![0u8; VARIABLE_L1_HEADER_LEN]
}

pub(crate) fn push_variable_l1_entry(page: &mut Vec<u8>, key: &[u8], offset: u64) {
    push_key_field(page, key);
    page.extend_from_slice(&offset.to_le_bytes());
}

/// Stamps the header and zero-pads to `size`; a compact page passes its own length.
pub(crate) fn finish_variable_l1_page(
    page: &mut Vec<u8>,
    count: usize,
    size: usize,
) -> Result<(), ArchiveError> {
    if page.len() > size {
        return Err(ArchiveError::ContractViolation(format!(
            "an L1 page of {size} bytes cannot hold {} bytes",
            page.len()
        )));
    }

    let (Ok(size_field), Ok(count_field)) = (u32::try_from(size), u32::try_from(count)) else {
        return Err(ArchiveError::ContractViolation(format!(
            "an L1 page of {size} bytes and {count} keys does not fit its u32 fields"
        )));
    };

    page.resize(size, 0);
    page[0..4].copy_from_slice(&size_field.to_le_bytes());
    page[4..8].copy_from_slice(&count_field.to_le_bytes());
    Ok(())
}

/// The intervals of the whole chain, in order. They must be ordered and disjoint ACROSS frames: the
/// lookup is one binary search over all of them.
pub(crate) fn decode_variable_l0_chain<TKey: PrimaryKeyVariableSize>(
    frames: &[L0FrameRead],
) -> Result<Vec<VariableL0Interval<TKey>>, ArchiveError> {
    let mut intervals: Vec<VariableL0Interval<TKey>> = Vec::new();

    for frame in frames {
        let place = format!("the L0 frame at {}", frame.offset);
        let mut walk = EntryWalk::new(&frame.body, 0, &place);
        let mut read = 0;

        while let Some((from_at, from_len)) = walk.first_key()? {
            let (to_at, to_len) = walk.inner_key()?;
            let offset = walk.offset()?;

            let interval = VariableL0Interval {
                from: TKey::from_slice(&frame.body[from_at..from_at + from_len]),
                to: TKey::from_slice(&frame.body[to_at..to_at + to_len]),
                offset,
            };

            let ordered = interval.from <= interval.to
                && intervals
                    .last()
                    .is_none_or(|previous| previous.to < interval.from);

            if !ordered {
                return Err(ArchiveError::Corrupted(format!(
                    "L0 interval {} is not ordered after the previous one",
                    intervals.len()
                )));
            }

            intervals.push(interval);
            read += 1;
        }

        if read != frame.count {
            return Err(count_mismatch(&place, frame.count, read));
        }
    }

    Ok(intervals)
}

/// `(size, count)` of an L1 header. The smallest entry is a one-byte key: 1 + 1 + 8 bytes.
pub(crate) fn decode_variable_l1_header(
    header: &[u8],
    offset: u64,
) -> Result<(usize, usize), ArchiveError> {
    let size = le_u32(header, 0) as usize;
    let count = le_u32(header, 4) as usize;

    let fits = size > VARIABLE_L1_HEADER_LEN
        && count > 0
        && count
            .checked_mul(variable_l1_entry_len(1))
            .is_some_and(|least| least <= size - VARIABLE_L1_HEADER_LEN);

    if !fits {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 page at {offset} declares size {size} and count {count}"
        )));
    }

    Ok((size, count))
}

/// A whole page. Its keys must be strictly ascending, as many as its header says, and span exactly the
/// L0 interval that points at it, or a binary search would answer "absent" for a key that is there.
pub(crate) fn decode_variable_l1_page<TKey: PrimaryKeyVariableSize>(
    bytes: &[u8],
    interval: &VariableL0Interval<TKey>,
) -> Result<Vec<VariableL1Entry<TKey>>, ArchiveError> {
    let offset = interval.offset;
    let (size, count) = decode_variable_l1_header(bytes, offset)?;

    if bytes.len() < size {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 page at {offset} is cut short"
        )));
    }

    let place = format!("the L1 page at {offset}");
    let mut walk = EntryWalk::new(&bytes[..size], VARIABLE_L1_HEADER_LEN, &place);
    let mut entries: Vec<VariableL1Entry<TKey>> = Vec::with_capacity(count);

    while let Some((key_at, key_len)) = walk.first_key()? {
        let data_offset = walk.offset()?;
        let key = TKey::from_slice(&bytes[key_at..key_at + key_len]);

        if entries.last().is_some_and(|previous| previous.key >= key) {
            return Err(ArchiveError::Corrupted(format!(
                "the keys of the L1 page at {offset} are not ascending"
            )));
        }

        entries.push(VariableL1Entry {
            key,
            key_at,
            key_len,
            offset: data_offset,
        });
    }

    if entries.len() != count {
        return Err(count_mismatch(&place, count, entries.len()));
    }

    let spans = entries
        .first()
        .is_some_and(|first| first.key == interval.from)
        && entries.last().is_some_and(|last| last.key == interval.to);

    if !spans {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 page at {offset} does not span its L0 interval"
        )));
    }

    Ok(entries)
}
