use crate::{
    ArchiveError, ArchiveFileReader, CheckedSlotIndex, DEFAULT_L1_CAPACITY,
    FixedKeyPreallocatedSlotsArchiveReader, IoReaderFactory, PreallocatedDataItem,
    PreallocatedSlotKey, PrimaryKeyFixedSize, decode_split_l1_chain, read_split_range_items,
    split_entries_between, split_l1_page_len,
};

impl<TPrimaryKey, TSlotKey, TItem, TFactory, TReader>
    FixedKeyPreallocatedSlotsArchiveReader<TPrimaryKey, TSlotKey, TItem, TFactory, TReader>
where
    TPrimaryKey: PrimaryKeyFixedSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TFactory: IoReaderFactory<TReader>,
    TReader: ArchiveFileReader,
{
    /// Every key `k` of the archive with `from <= k <= to`, in ascending key order, each with the items of
    /// its slots `from_slot..=to_slot` exactly as [`Self::get`] gives them. The whole range comes back at
    /// once.
    ///
    /// * `from > to` — `Ok(vec![])` at once, WITHOUT reading anything;
    /// * `from_slot > to_slot` — every key in the range with no items, reading no data page;
    /// * `Err` — a read failed, a page is corrupted, or a slot index is out of range. Never a partial `Ok`.
    ///
    /// The L0 intervals the range touches are found in memory. Their L1 pages go through the L1 cache as
    /// `get`'s do — a cached page is used, and must span its interval exactly like a page just read — and
    /// the pages that are not cached and lie back to back in the index are read with ONE `read_at` per run
    /// of at most 8 MiB, each page checked as a read of it alone checks it, and cached. So a wide range of a
    /// compact archive costs a handful of index reads, and the same range again, with a cache holding its
    /// pages, none.
    ///
    /// Data pages do not go through the data cache. They are read in FILE order: the pages that follow one
    /// another in a data file are read together, one `read_at` per run of at most 8 MiB, and the items are
    /// handed back in key order. A contiguous range of a compact archive, and a dense range of a working
    /// one whatever order its keys were written in, is a handful of reads.
    ///
    /// Memory: every key of the range with `(to_slot − from_slot + 1) × size_of::<TItem>()` of items, plus
    /// one run of at most 8 MiB at a time.
    pub async fn get_range(
        &self,
        from: &TPrimaryKey,
        to: &TPrimaryKey,
        from_slot: TSlotKey,
        to_slot: TSlotKey,
    ) -> Result<Vec<(TPrimaryKey, Vec<TItem>)>, ArchiveError> {
        let from_slot = from_slot.checked_slot_index()?;
        let to_slot = to_slot.checked_slot_index()?;

        if from > to {
            return Ok(Vec::new());
        }

        let entries = split_entries_between(
            &self.index,
            &self.l1_cache,
            &self.l0,
            from,
            to,
            split_l1_page_len::<TPrimaryKey>(DEFAULT_L1_CAPACITY),
            decode_split_l1_chain,
        )
        .await?;

        read_split_range_items::<_, TSlotKey, TItem, _, _>(
            &self.data_files,
            &self.factory,
            entries,
            from_slot,
            to_slot,
        )
        .await
    }
}
