//! The data page every module of this crate shares: one page per primary key, preallocated for every
//! slot.
//!
//! ```text
//! size u32 · next u64 (0) · item_len u16    then SLOTS × item_len
//! ```

use tokio::io::{AsyncRead, AsyncSeek};

use crate::{
    ArchiveError, CheckedItemBytes, PreallocatedDataItem, PreallocatedSlotKey, le_u16, le_u32,
    le_u64, read_exact_at,
};

pub(crate) const DATA_HEADER_LEN: usize = 14;

pub(crate) fn data_page_len<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>() -> usize {
    DATA_HEADER_LEN + TSlot::SLOTS * TItem::ITEM_LEN
}

/// The slot and item sizes every data page header stores must fit their fields. A type whose constants
/// break that can never be written, so it is refused before the first byte.
pub(crate) fn check_data_page_sizes<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>() {
    assert!(
        TSlot::SLOTS > 0,
        "PreallocatedSlotKey::SLOTS must be positive"
    );
    assert!(
        TItem::ITEM_LEN > 0 && TItem::ITEM_LEN <= u16::MAX as usize,
        "PreallocatedDataItem::ITEM_LEN must fit u16"
    );
    assert!(
        u32::try_from(data_page_len::<TSlot, TItem>()).is_ok(),
        "a data page must fit its u32 size"
    );
}

pub(crate) fn data_page_header<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>()
-> [u8; DATA_HEADER_LEN] {
    let mut header = [0u8; DATA_HEADER_LEN];
    header[0..4].copy_from_slice(&(data_page_len::<TSlot, TItem>() as u32).to_le_bytes());
    header[12..14].copy_from_slice(&(TItem::ITEM_LEN as u16).to_le_bytes());
    header
}

/// A new page: the header, then `TItem::default()` in every slot.
pub(crate) fn empty_data_page<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>()
-> Result<Vec<u8>, ArchiveError> {
    let mut page = Vec::with_capacity(data_page_len::<TSlot, TItem>());
    page.extend_from_slice(&data_page_header::<TSlot, TItem>());

    let empty = TItem::default();
    for _ in 0..TSlot::SLOTS {
        empty.copy_item_checked_to(&mut page)?;
    }

    Ok(page)
}

/// Where the item of `slot` starts inside a data page.
pub(crate) fn slot_at<TItem: PreallocatedDataItem>(slot: usize) -> usize {
    DATA_HEADER_LEN + slot * TItem::ITEM_LEN
}

/// The items of slots `from..=to` of a page [`check_data_page`] accepted.
pub(crate) fn items_of_slots<TItem: PreallocatedDataItem>(
    page: &[u8],
    from: usize,
    to: usize,
) -> Vec<TItem> {
    (from..=to)
        .map(|slot| {
            let at = slot_at::<TItem>(slot);
            TItem::from_slice(&page[at..at + TItem::ITEM_LEN])
        })
        .collect()
}

/// A data page must be of exactly these slot and item sizes; a file of other sizes is not this file.
pub(crate) fn check_data_page<TSlot: PreallocatedSlotKey, TItem: PreallocatedDataItem>(
    bytes: &[u8],
    offset: u64,
) -> Result<(), ArchiveError> {
    let expected = data_page_len::<TSlot, TItem>();

    if bytes.len() < DATA_HEADER_LEN {
        return Err(ArchiveError::Corrupted(format!(
            "the data page at {offset} is cut short"
        )));
    }

    let size = le_u32(bytes, 0) as usize;
    let next = le_u64(bytes, 4);
    let item_len = le_u16(bytes, 12) as usize;

    if size != expected || next != 0 || item_len != TItem::ITEM_LEN || bytes.len() < expected {
        return Err(ArchiveError::Corrupted(format!(
            "the data page at {offset} declares size {size}, next {next}, item_len {item_len}; \
             expected {expected}, 0, {}",
            TItem::ITEM_LEN
        )));
    }

    Ok(())
}

/// One whole data page from `offset`, checked.
pub(crate) async fn read_data_page<TSlot, TItem, TSource>(
    source: &mut TSource,
    offset: u64,
) -> Result<Vec<u8>, ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TSource: AsyncRead + AsyncSeek + Unpin,
{
    let mut bytes = vec![0u8; data_page_len::<TSlot, TItem>()];
    read_exact_at(source, offset, &mut bytes).await?;
    check_data_page::<TSlot, TItem>(&bytes, offset)?;
    Ok(bytes)
}
