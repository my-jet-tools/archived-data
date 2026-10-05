use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, COMPACT_L1_CAPACITY, CheckedKeyBytes,
    CompactDataFilesWriter, CompactIndexWriter, CompactL1PagePlan, CompactionPlan, DataFileLayout,
    DataRef, FixedKeyPreallocatedSlotsArchiveReader, IoReaderFactory, IoWriterFactory,
    PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyFixedSize, SPLIT_L0_HEADER_LEN,
    encode_split_l0_frame, encode_split_l1_page, refuse_unordered, source_changed_while_compacting,
    split_data_page_len, split_l0_frame_len, split_l1_page_len,
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
    /// What [`Self::write_compacted_to`] will write, from one pass over the L1 pages (none through the
    /// caches): the compact L1 pages, the exact size of the compact index, and the exact size of every
    /// compact data file under the reader's `max_data_file_bytes`. An empty archive plans a 20-byte index
    /// and no data files.
    ///
    /// `max_data_file_bytes` decides which data file every page lands in, so it must be the same for every
    /// run over one archive: that is the one condition under which two runs write identical files.
    ///
    /// Keys that do not ascend across L1 pages are `Corrupted`; an index past 4 GiB or data needing data
    /// file 256 is a `ContractViolation`.
    pub async fn compaction_plan(&self) -> Result<CompactionPlan, ArchiveError> {
        let mut ranges: Vec<(TPrimaryKey, TPrimaryKey, usize)> = Vec::new();
        let mut previous: Option<TPrimaryKey> = None;

        for interval in &self.l0 {
            for (key, _) in self.source_l1_page(interval).await? {
                refuse_unordered(&previous, &key)?;

                match ranges.last_mut() {
                    Some((_, to, count)) if *count < COMPACT_L1_CAPACITY => {
                        *to = key.clone();
                        *count += 1;
                    }
                    _ => ranges.push((key.clone(), key.clone(), 1)),
                }

                previous = Some(key);
            }
        }

        let len = split_l1_page_len::<TPrimaryKey>(COMPACT_L1_CAPACITY);
        let mut pages = Vec::with_capacity(ranges.len());

        for (from, to, count) in ranges {
            pages.push(CompactL1PagePlan {
                from: key_bytes(&from)?,
                to: key_bytes(&to)?,
                count,
                len,
            });
        }

        let l0_len = split_l0_frame_len::<TPrimaryKey>(pages.len()) as u64;
        let layout = DataFileLayout::new(
            split_data_page_len::<TSlotKey, TItem>(),
            self.max_data_file_bytes(),
        );

        CompactionPlan::new(pages, l0_len, layout)
    }

    /// Writes the COMPACT archive of `plan` through `out`: each data file in order, append only, each
    /// finished before the next is created; then the index, append only, finished LAST.
    ///
    /// ONE L0 frame of exact size (`next` 0) · full L1 pages of [`COMPACT_L1_CAPACITY`] keys, the last
    /// zero-padded · the data page of the key of rank `i` at `ref(i)` of the plan's layout, as one frame.
    /// The bytes are a function of the keys, the items and the plan's `max_data_file_bytes` only, so a
    /// run over the same archive with the same `max_data_file_bytes` writes identical files.
    ///
    /// The archive is complete when its index exists AND opens — not when the index object exists. An `Err`
    /// in the data pass leaves no index; one in the index pass can leave a short index behind — a file
    /// writer dropped unfinished still persists what reached it, and an append that failed partway may
    /// leave part of a page — which `open` refuses as `Corrupted`: its L0 names an L1 page that starts, or
    /// its last L1 page ends, past the end of the index.
    ///
    /// Two more passes over the source's L1 pages (data, index), none through the caches; memory is one
    /// L1 page plus one data page. A source that disagrees with the plan is `Corrupted`.
    pub async fn write_compacted_to<TOut, TWriter>(
        &self,
        plan: &CompactionPlan,
        out: &TOut,
    ) -> Result<(), ArchiveError>
    where
        TOut: IoWriterFactory<TWriter>,
        TWriter: ArchiveFileWriter,
    {
        let mut data = CompactDataFilesWriter::<TSlotKey, TItem, TWriter>::new(plan);

        for interval in &self.l0 {
            for (_, at) in self.source_l1_page(interval).await? {
                let body = self.source_data_body(at).await?;
                data.push(out, &body).await?;
            }
        }

        data.finish().await?;

        let mut index = CompactIndexWriter::create(out, plan, &compact_l0_frame(plan)?).await?;

        let mut page: Vec<(TPrimaryKey, DataRef)> = Vec::with_capacity(COMPACT_L1_CAPACITY);
        let mut previous: Option<TPrimaryKey> = None;
        let mut rank = 0;

        for interval in &self.l0 {
            for (key, _) in self.source_l1_page(interval).await? {
                if rank >= plan.keys {
                    return Err(source_changed_while_compacting());
                }

                refuse_unordered(&previous, &key)?;
                page.push((key.clone(), plan.layout.ref_of(rank)?));
                previous = Some(key);
                rank += 1;

                if page.len() == COMPACT_L1_CAPACITY {
                    push_l1_page(&mut index, &page).await?;
                    page.clear();
                }
            }
        }

        if !page.is_empty() {
            push_l1_page(&mut index, &page).await?;
        }

        index.finish().await
    }
}

fn key_bytes<TKey: PrimaryKeyFixedSize>(key: &TKey) -> Result<Vec<u8>, ArchiveError> {
    let mut bytes = Vec::with_capacity(TKey::LEN);
    key.copy_key_checked_to(&mut bytes)?;
    Ok(bytes)
}

/// The one L0 frame: every planned page's first and last key and where it starts.
fn compact_l0_frame(plan: &CompactionPlan) -> Result<Vec<u8>, ArchiveError> {
    let mut body = Vec::new();

    for (page, offset) in plan.pages.iter().zip(plan.l1_offsets()) {
        body.extend_from_slice(&page.from);
        body.extend_from_slice(&page.to);
        body.extend_from_slice(&offset.to_le_bytes());
    }

    encode_split_l0_frame(SPLIT_L0_HEADER_LEN + body.len(), 0, plan.pages.len(), &body)
}

async fn push_l1_page<TKey: PrimaryKeyFixedSize, TWriter: ArchiveFileWriter>(
    index: &mut CompactIndexWriter<'_, TWriter>,
    page: &[(TKey, DataRef)],
) -> Result<(), ArchiveError> {
    let bytes = encode_split_l1_page(page, COMPACT_L1_CAPACITY)?;
    let from = key_bytes(&page[0].0)?;
    let to = key_bytes(&page[page.len() - 1].0)?;

    index.push_l1_page(&bytes, page.len(), &from, &to).await
}
