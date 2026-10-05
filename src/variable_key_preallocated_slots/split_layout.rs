use crate::{
    ArchiveError, DATA_REF_LEN, DataRef, EntryWalk, L0FrameBody, MAX_VARIABLE_KEY_LEN,
    PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyVariableSize, SPLIT_L0_HEADER_LEN,
    SPLIT_L1_HEADER_LEN, SplitFrame, SplitFrameHeader, SplitL0Interval,
    check_split_data_page_sizes, count_mismatch, key_prefix_len, push_key_field,
};

/// An L1 page is addressed by a `u32` offset inside the index.
const L1_OFFSET_BYTES: usize = 4;

/// One entry of an L1 page as read: the key, where its raw bytes are — `frames[frame].body[key_at..]` of
/// the chain it was decoded from — and its data page.
pub(crate) struct VariableSplitL1Entry<TKey> {
    pub key: TKey,
    pub frame: usize,
    pub key_at: usize,
    pub key_len: usize,
    pub data: DataRef,
}

/// prefix + key + data_ref.
pub(crate) fn variable_split_l1_entry_len(key_len: usize) -> usize {
    key_prefix_len(key_len) + key_len + DATA_REF_LEN
}

/// prefix + from + prefix + to + l1_offset.
pub(crate) fn variable_split_l0_entry_len(from_len: usize, to_len: usize) -> usize {
    key_prefix_len(from_len) + from_len + key_prefix_len(to_len) + to_len + L1_OFFSET_BYTES
}

/// `2 × entry ≤ l1_page_bytes − 12`. At most half the room of a page is what makes a halving by bytes
/// always produce exactly two pages that fit.
pub(crate) fn max_variable_split_l1_entry_len(l1_page_bytes: usize) -> usize {
    (l1_page_bytes - SPLIT_L1_HEADER_LEN) / 2
}

pub(crate) fn refuse_split_oversized_key(
    key_len: usize,
    l1_page_bytes: usize,
) -> Result<(), ArchiveError> {
    let entry = variable_split_l1_entry_len(key_len);
    let max = max_variable_split_l1_entry_len(l1_page_bytes);

    if entry > max {
        return Err(ArchiveError::ContractViolation(format!(
            "key too large: {key_len} bytes make an L1 entry of {entry} bytes, and an L1 page of \
             {l1_page_bytes} bytes accepts at most {max}"
        )));
    }

    Ok(())
}

/// Sizes that can never be written are refused before the first byte.
pub(crate) fn check_variable_split_sizes<TSlot, TItem>(l0_frame_bytes: usize, l1_page_bytes: usize)
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
{
    check_split_data_page_sizes::<TSlot, TItem>();
    assert!(
        l1_page_bytes >= SPLIT_L1_HEADER_LEN + 2 * variable_split_l1_entry_len(1),
        "an L1 page must hold two entries of a one-byte key"
    );
    assert!(
        u32::try_from(l1_page_bytes).is_ok() && u32::try_from(l0_frame_bytes).is_ok(),
        "an L0 frame and an L1 page must fit their u32 size"
    );

    let largest_key = (max_variable_split_l1_entry_len(l1_page_bytes) - DATA_REF_LEN)
        .min(key_prefix_len(MAX_VARIABLE_KEY_LEN) + MAX_VARIABLE_KEY_LEN);
    assert!(
        l0_frame_bytes >= SPLIT_L0_HEADER_LEN + 2 * largest_key + L1_OFFSET_BYTES,
        "an L0 frame must hold one interval of the largest key its L1 pages accept"
    );
}

/// `(from, to, l1_offset)` packed greedily into frame bodies of at most `body_capacity` bytes. Every
/// entry fits a body on its own — [`check_variable_split_sizes`] and [`refuse_split_oversized_key`] see
/// to it.
pub(crate) fn encode_variable_split_l0_bodies<'a>(
    entries: impl Iterator<Item = (&'a [u8], &'a [u8], u32)>,
    body_capacity: usize,
) -> Vec<L0FrameBody> {
    let mut bodies: Vec<L0FrameBody> = Vec::new();

    for (from, to, l1_offset) in entries {
        let entry_len = variable_split_l0_entry_len(from.len(), to.len());

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
        body.body.extend_from_slice(&l1_offset.to_le_bytes());
        body.count += 1;
    }

    bodies
}

/// A number of frames never below what [`encode_variable_split_l0_bodies`] packs `intervals` entries into,
/// when they total `entry_bytes`, none is longer than `longest_entry`, and a body holds `body_capacity`
/// bytes. It is the smaller of two bounds, each of which on its own is never too few:
///
/// * by COUNT — a frame closes only when the next entry does not fit, so every closed frame holds at least
///   `floor(body_capacity / longest_entry)` entries: `ceil(intervals / that)`;
/// * by BYTES — the same close leaves every closed frame holding more than `body_capacity − longest_entry`
///   bytes, and the closed frames — all but the last — hold `entry_bytes` at most between them:
///   `1 + floor(entry_bytes / (body_capacity − longest_entry + 1))`.
///
/// Each one alone can be far too many. By count, one long key counts every interval as long as its own;
/// by bytes, a longest entry near a whole body leaves almost no room it can count on. Where one is loose
/// the other is not.
pub(crate) fn variable_split_l0_frames_bound(
    intervals: usize,
    entry_bytes: usize,
    longest_entry: usize,
    body_capacity: usize,
) -> usize {
    let by_count = intervals.div_ceil(body_capacity / longest_entry).max(1);
    let by_bytes = 1 + entry_bytes / (body_capacity - longest_entry + 1);

    by_count.min(by_bytes)
}

pub(crate) fn push_variable_split_l1_entry(body: &mut Vec<u8>, key: &[u8], data: DataRef) {
    push_key_field(body, key);
    data.encode_to(body);
}

/// The intervals of the whole chain, in order. Every frame's entries end at the end marker or at its
/// end and must be exactly its `count`; the intervals must be ordered and disjoint ACROSS frames, because
/// the lookup is one binary search over all of them.
pub(crate) fn decode_variable_split_l0_chain<TKey: PrimaryKeyVariableSize>(
    frames: &[SplitFrame],
) -> Result<Vec<SplitL0Interval<TKey>>, ArchiveError> {
    let mut intervals: Vec<SplitL0Interval<TKey>> = Vec::new();

    for frame in frames {
        let place = format!("the L0 frame at {}", frame.header.offset);
        let mut walk = EntryWalk::new(&frame.body, 0, &place);
        let mut read = 0;

        while let Some((from_at, from_len)) = walk.first_key()? {
            let (to_at, to_len) = walk.inner_key()?;
            let l1_offset = walk.u32_field()?;

            let interval = SplitL0Interval {
                from: TKey::from_slice(&frame.body[from_at..from_at + from_len]),
                to: TKey::from_slice(&frame.body[to_at..to_at + to_len]),
                l1_offset,
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

        if read != frame.header.field as usize {
            return Err(count_mismatch(&place, frame.header.field as usize, read));
        }
    }

    Ok(intervals)
}

/// The `count` of one L1 frame. Entries never straddle frames, so every frame is checked on its own: at
/// least one entry, and room for `count` entries of a one-byte key.
pub(crate) fn check_variable_split_l1_frame_header(
    header: &SplitFrameHeader,
) -> Result<usize, ArchiveError> {
    let count = header.field as usize;

    let fits = count > 0
        && count
            .checked_mul(variable_split_l1_entry_len(1))
            .is_some_and(|least| least <= header.size - SPLIT_L1_HEADER_LEN);

    if !fits {
        return Err(ArchiveError::Corrupted(format!(
            "the L1 frame at {} declares size {} and count {count}",
            header.offset, header.size
        )));
    }

    Ok(count)
}

/// The page chained over `frames`. Each frame's entries end at the end marker or at its end and must be
/// exactly its `count`; the keys must be strictly ascending across the chain and span exactly the L0
/// interval that points at it, or a binary search would answer "absent" for a key that is there.
pub(crate) fn decode_variable_split_l1_chain<TKey: PrimaryKeyVariableSize>(
    frames: &[SplitFrame],
    interval: &SplitL0Interval<TKey>,
) -> Result<Vec<VariableSplitL1Entry<TKey>>, ArchiveError> {
    let offset = interval.l1_offset;
    let mut entries: Vec<VariableSplitL1Entry<TKey>> = Vec::new();

    for (index, frame) in frames.iter().enumerate() {
        let count = check_variable_split_l1_frame_header(&frame.header)?;
        let place = format!("the L1 frame at {}", frame.header.offset);
        let mut walk = EntryWalk::new(&frame.body, 0, &place);
        let mut read = 0;

        while let Some((key_at, key_len)) = walk.first_key()? {
            let data = walk.data_ref()?;
            let key = TKey::from_slice(&frame.body[key_at..key_at + key_len]);

            if entries.last().is_some_and(|previous| previous.key >= key) {
                return Err(ArchiveError::Corrupted(format!(
                    "the keys of the L1 page at {offset} are not ascending"
                )));
            }

            entries.push(VariableSplitL1Entry {
                key,
                frame: index,
                key_at,
                key_len,
                data,
            });
            read += 1;
        }

        if read != count {
            return Err(count_mismatch(&place, count, read));
        }
    }

    interval.check_spanned_by(
        entries.first().map(|entry| &entry.key),
        entries.last().map(|entry| &entry.key),
    )?;

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_items::seeded;

    /// The frames `lens` — `(from, to)` key lengths — pack into, and what the bound says.
    fn packed_and_bound(lens: &[(usize, usize)], body_capacity: usize) -> (usize, usize) {
        let keys: Vec<(Vec<u8>, Vec<u8>)> = lens
            .iter()
            .map(|(from, to)| (vec![b'k'; *from], vec![b'k'; *to]))
            .collect();

        let packed = encode_variable_split_l0_bodies(
            keys.iter().map(|(from, to)| (&from[..], &to[..], 7)),
            body_capacity,
        )
        .len()
        .max(1);

        let longest = lens
            .iter()
            .map(|(from, to)| *from.max(to))
            .max()
            .unwrap_or(1);
        let entry_bytes = lens
            .iter()
            .map(|(from, to)| variable_split_l0_entry_len(*from, *to))
            .sum();

        let bound = variable_split_l0_frames_bound(
            lens.len(),
            entry_bytes,
            variable_split_l0_entry_len(longest, longest),
            body_capacity,
        );

        (packed, bound)
    }

    #[test]
    fn the_bound_is_never_below_the_frames_the_entries_pack_into() {
        let mut rng = seeded(0x10);

        for round in 0..2_000 {
            let body_capacity = 300 + (rng.next() % 4_000) as usize;
            let max_key = 1 + (rng.next() % ((body_capacity as u64 - 8) / 2).min(600)) as usize;
            let count = (rng.next() % 400) as usize;

            let lens: Vec<(usize, usize)> = (0..count)
                .map(|_| match rng.next() % 3 {
                    0 => (max_key, 1 + (rng.next() % max_key as u64) as usize),
                    _ => (
                        1 + (rng.next() % 12) as usize,
                        1 + (rng.next() % 12) as usize,
                    ),
                })
                .filter(|(from, to)| variable_split_l0_entry_len(*from, *to) <= body_capacity)
                .collect();

            let (packed, bound) = packed_and_bound(&lens, body_capacity);
            assert!(
                packed <= bound,
                "round {round}: {} entries in {body_capacity}-byte bodies pack into {packed} frames, \
                 bound {bound}",
                lens.len()
            );
        }
    }

    #[test]
    fn one_long_entry_costs_about_one_frame_not_half_of_every_frame() {
        // 4,076 bytes a body; 22-byte entries of 8-byte keys, 185 to a frame.
        let short: Vec<(usize, usize)> = vec![(8, 8); 980];
        assert_eq!(packed_and_bound(&short, 4_076), (6, 6));

        // One interval from a 499-byte key: by count alone four entries a frame, 245 frames.
        let mut long = short.clone();
        long[0] = (499, 8);
        let (packed, bound) = packed_and_bound(&long, 4_076);
        assert_eq!(packed, 6);
        assert_eq!(bound, 8);
    }
}
