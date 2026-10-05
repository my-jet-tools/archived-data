//! The data page of the split archive: one per primary key, preallocated for every slot, in any data
//! file.
//!
//! ```text
//! size u32 · next u32 · item_len u16    then SLOTS × item_len
//! ```
//!
//! Writers always write ONE frame, `next` 0. A reader follows `next` all the same: the page is then the
//! concatenation of the bodies of its chain, every frame must state the same `item_len`, and the bodies
//! must total exactly `SLOTS × ITEM_LEN`.

use crate::{
    ArchiveError, ArchiveFileReader, CheckedItemBytes, PreallocatedDataItem, PreallocatedSlotKey,
    SPLIT_DATA_HEADER_LEN, SizedReader, SplitFrame, SplitFrameKind, read_split_chain,
};

pub(crate) fn split_data_body_len<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>() -> usize
{
    TSlot::SLOTS * TItem::ITEM_LEN
}

pub(crate) fn split_data_page_len<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>() -> usize
{
    SPLIT_DATA_HEADER_LEN + split_data_body_len::<TSlot, TItem>()
}

/// The slot and item sizes a data page header stores must fit their fields. A type whose constants
/// break that can never be written, so it is refused before the first byte.
pub(crate) fn check_split_data_page_sizes<
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
>() {
    assert!(
        TSlot::SLOTS > 0,
        "PreallocatedSlotKey::SLOTS must be positive"
    );
    assert!(
        TItem::ITEM_LEN > 0 && TItem::ITEM_LEN <= u16::MAX as usize,
        "PreallocatedDataItem::ITEM_LEN must fit u16"
    );
    assert!(
        TSlot::SLOTS
            .checked_mul(TItem::ITEM_LEN)
            .and_then(|body| body.checked_add(SPLIT_DATA_HEADER_LEN))
            .is_some_and(|page| u32::try_from(page).is_ok()),
        "a data page must fit its u32 size"
    );
}

/// ONE frame holding `body`, which must be exactly `SLOTS × ITEM_LEN` bytes.
pub(crate) fn encode_split_data_page<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>(
    body: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    let expected = split_data_body_len::<TSlot, TItem>();

    if body.len() != expected {
        return Err(ArchiveError::ContractViolation(format!(
            "a data page body of {} bytes; SLOTS × ITEM_LEN is {expected}",
            body.len()
        )));
    }

    let mut page = split_data_page_header::<TSlot, TItem>().to_vec();
    page.extend_from_slice(body);
    Ok(page)
}

/// A new page: the header, then `TItem::default()` in every slot.
pub(crate) fn empty_split_data_page<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>()
-> Result<Vec<u8>, ArchiveError> {
    let mut page = Vec::with_capacity(split_data_page_len::<TSlot, TItem>());
    page.extend_from_slice(&split_data_page_header::<TSlot, TItem>());

    let empty = TItem::default();
    for _ in 0..TSlot::SLOTS {
        empty.copy_item_checked_to(&mut page)?;
    }

    Ok(page)
}

/// Where the item of `slot` starts inside a whole ONE-frame page.
pub(crate) fn split_slot_at<TItem: PreallocatedDataItem>(slot: usize) -> usize {
    SPLIT_DATA_HEADER_LEN + slot * TItem::ITEM_LEN
}

/// The items of slots `from..=to` of a body [`read_split_data_page`] returned.
pub(crate) fn items_of_split_body<TItem: PreallocatedDataItem>(
    body: &[u8],
    from: usize,
    to: usize,
) -> Vec<TItem> {
    (from..=to)
        .map(|slot| {
            let at = slot * TItem::ITEM_LEN;
            TItem::from_slice(&body[at..at + TItem::ITEM_LEN])
        })
        .collect()
}

/// The body of the page chained over `frames`, checked.
pub(crate) fn decode_split_data_frames<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>(
    frames: &[SplitFrame],
) -> Result<Vec<u8>, ArchiveError> {
    let expected = split_data_body_len::<TSlot, TItem>();
    let offset = frames.first().map_or(0, |frame| frame.header.offset);
    let mut body = Vec::with_capacity(expected);

    for frame in frames {
        if frame.header.field as usize != TItem::ITEM_LEN {
            return Err(ArchiveError::Corrupted(format!(
                "the data frame at {} declares item_len {}, expected {}",
                frame.header.offset,
                frame.header.field,
                TItem::ITEM_LEN
            )));
        }

        if body.len() + frame.body.len() > expected {
            return Err(ArchiveError::Corrupted(format!(
                "the data page at {offset} holds more than its {expected} bytes of items"
            )));
        }

        body.extend_from_slice(&frame.body);
    }

    if body.len() != expected {
        return Err(ArchiveError::Corrupted(format!(
            "the data page at {offset} holds {} of its {expected} bytes of items",
            body.len()
        )));
    }

    Ok(body)
}

/// The body of the page at `offset` of a data file, following its continuations: one `read_at` for a
/// page written as one frame.
pub(crate) async fn read_split_data_page<TSlot, TItem, TReader>(
    reader: &SizedReader<TReader>,
    offset: u32,
) -> Result<Vec<u8>, ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TReader: ArchiveFileReader,
{
    let guess = split_data_page_len::<TSlot, TItem>();
    let frames = read_split_chain(reader, offset as u64, SplitFrameKind::Data, guess).await?;
    decode_split_data_frames::<TSlot, TItem>(&frames)
}

fn split_data_page_header<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>()
-> [u8; SPLIT_DATA_HEADER_LEN] {
    let mut header = [0u8; SPLIT_DATA_HEADER_LEN];
    header[0..4].copy_from_slice(&(split_data_page_len::<TSlot, TItem>() as u32).to_le_bytes());
    header[8..10].copy_from_slice(&(TItem::ITEM_LEN as u16).to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_memory_io::{FileRole, InMemoryIo};
    use crate::{IoReaderFactory, TinyItem, TinySlot, ZeroSlots, data_frame};

    const BODY: [u8; 12] = [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];

    async fn read(bytes: Vec<u8>, offset: u32) -> (Result<Vec<u8>, ArchiveError>, usize) {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Data(0), bytes);
        let reader = SizedReader::open(io.create_data_reader(0).await.unwrap())
            .await
            .unwrap();

        let body = read_split_data_page::<TinySlot, TinyItem, _>(&reader, offset).await;
        (body, file.read_ats())
    }

    #[test]
    fn an_empty_page_is_the_header_then_the_default_item_in_every_slot() {
        check_split_data_page_sizes::<TinySlot, TinyItem>();

        assert_eq!(split_data_page_len::<TinySlot, TinyItem>(), 22);
        assert_eq!(split_slot_at::<TinyItem>(2), 18);
        assert_eq!(
            empty_split_data_page::<TinySlot, TinyItem>().unwrap(),
            data_frame(22, 0, 4, &[0xEE; 12])
        );
        assert_eq!(
            encode_split_data_page::<TinySlot, TinyItem>(&BODY).unwrap(),
            data_frame(22, 0, 4, &BODY)
        );
    }

    #[test]
    fn a_body_of_the_wrong_length_is_not_encoded() {
        for body in [&BODY[..11], &[0; 13][..]] {
            assert!(matches!(
                encode_split_data_page::<TinySlot, TinyItem>(body),
                Err(ArchiveError::ContractViolation(_))
            ));
        }
    }

    #[test]
    #[should_panic(expected = "SLOTS must be positive")]
    fn a_slot_type_without_slots_panics() {
        check_split_data_page_sizes::<ZeroSlots, TinyItem>();
    }

    #[tokio::test]
    async fn a_one_frame_page_is_one_read_at_and_gives_its_items_back() {
        let mut bytes = vec![0xCC; 5];
        bytes.extend_from_slice(&data_frame(22, 0, 4, &BODY));

        let (body, read_ats) = read(bytes, 5).await;
        let body = body.unwrap();

        assert_eq!(body, BODY);
        assert_eq!(read_ats, 1);
        assert_eq!(
            items_of_split_body::<TinyItem>(&body, 1, 2),
            [TinyItem([2; 4]), TinyItem([3; 4])]
        );
    }

    #[tokio::test]
    async fn a_page_chained_over_three_frames_is_their_bodies_joined() {
        let mut bytes = data_frame(15, 30, 4, &BODY[0..5]);
        bytes.resize(30, 0xCC);
        bytes.extend_from_slice(&data_frame(14, 50, 4, &BODY[5..9]));
        bytes.resize(50, 0xCC);
        bytes.extend_from_slice(&data_frame(13, 0, 4, &BODY[9..12]));

        assert_eq!(read(bytes, 0).await.0.unwrap(), BODY);
    }

    #[tokio::test]
    async fn a_short_or_long_body_or_a_wrong_item_len_is_corrupted() {
        let short = data_frame(18, 0, 4, &BODY[..8]);

        let mut long = data_frame(22, 26, 4, &BODY);
        long.extend_from_slice(&data_frame(14, 0, 4, &[0; 4]));

        let first_item_len = data_frame(22, 0, 5, &BODY);

        let mut continuation_item_len = data_frame(16, 16, 4, &BODY[..6]);
        continuation_item_len.extend_from_slice(&data_frame(16, 0, 5, &BODY[6..]));

        for bytes in [short, long, first_item_len, continuation_item_len] {
            assert!(matches!(
                read(bytes, 0).await.0,
                Err(ArchiveError::Corrupted(_))
            ));
        }
    }
}
