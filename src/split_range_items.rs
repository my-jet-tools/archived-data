use crate::{
    ArchiveError, ArchiveFileReader, DataRef, IoReaderFactory, LazyDataReaders,
    PreallocatedDataItem, PreallocatedSlotKey, items_of_split_body, read_split_data_bodies,
};

/// What a split reader's `get_range` answers for the L1 entries it found: every key, in the order given,
/// with the items of its slots `from_slot..=to_slot`. `from_slot > to_slot` is every key with no items,
/// reading no data page; otherwise the data pages are read in runs, through no cache, and a failed read or
/// a damaged page anywhere is the `Err`, never a partial answer.
///
/// The pages are read in FILE order — the refs sorted by `(file_no, offset)` — and handed back in the order
/// of the entries. A working archive places a key's page when the key first arrives, so keys in key order
/// point all over their data files; in file order the pages of a dense range lie back to back and are read
/// in runs, as a compact archive's are, instead of one `read_at` per key.
pub(crate) async fn read_split_range_items<TKey, TSlot, TItem, TReader, TFactory>(
    data_files: &LazyDataReaders<TReader>,
    factory: &TFactory,
    entries: Vec<(TKey, DataRef)>,
    from_slot: usize,
    to_slot: usize,
) -> Result<Vec<(TKey, Vec<TItem>)>, ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TReader: ArchiveFileReader,
    TFactory: IoReaderFactory<TReader>,
{
    if from_slot > to_slot {
        return Ok(entries
            .into_iter()
            .map(|(key, _)| (key, Vec::new()))
            .collect());
    }

    // Positions into `entries`, in file order; stable, so equal refs keep their key order.
    let mut in_file_order: Vec<usize> = (0..entries.len()).collect();
    in_file_order.sort_by_key(|position| entries[*position].1);

    let refs: Vec<DataRef> = in_file_order
        .iter()
        .map(|position| entries[*position].1)
        .collect();

    let mut items: Vec<Vec<TItem>> = std::iter::repeat_with(Vec::new)
        .take(entries.len())
        .collect();
    let mut positions = in_file_order.into_iter();

    read_split_data_bodies::<TSlot, TItem, _, _>(data_files, factory, &refs, |body| {
        if let Some(position) = positions.next() {
            items[position] = items_of_split_body::<TItem>(body, from_slot, to_slot);
        }
    })
    .await?;

    Ok(entries.into_iter().map(|(key, _)| key).zip(items).collect())
}
