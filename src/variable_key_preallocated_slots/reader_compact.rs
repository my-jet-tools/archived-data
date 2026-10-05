use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, COMPACT_L1_MAX_KEYS, COMPACT_L1_PAGE_BYTES,
    CompactDataFilesWriter, CompactIndexWriter, CompactL1PagePlan, CompactionPlan, DataFileLayout,
    DataRef, IoReaderFactory, IoWriterFactory, PreallocatedDataItem, PreallocatedSlotKey,
    PrimaryKeyVariableSize, SPLIT_L0_HEADER_LEN, SPLIT_L1_HEADER_LEN,
    VariableKeyPreallocatedSlotsArchiveReader, encode_split_l0_frame, encode_split_l1_frame,
    encode_variable_split_l0_bodies, push_variable_split_l1_entry, refuse_unordered,
    source_changed_while_compacting, split_data_page_len, variable_split_l0_entry_len,
    variable_split_l1_entry_len,
};

/// How full a compact L1 page is, header included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SplitPageFill {
    count: usize,
    len: usize,
}

impl SplitPageFill {
    fn new() -> Self {
        Self {
            count: 0,
            len: SPLIT_L1_HEADER_LEN,
        }
    }

    /// A page closes at [`COMPACT_L1_MAX_KEYS`] keys, or before an entry that would take it past
    /// [`COMPACT_L1_PAGE_BYTES`]. An empty page takes any entry: a working archive written with larger L1
    /// pages may hold a key whose entry alone is past the limit, and it still needs a page.
    fn takes(&self, entry_len: usize) -> bool {
        self.count == 0
            || (self.count < COMPACT_L1_MAX_KEYS && self.len + entry_len <= COMPACT_L1_PAGE_BYTES)
    }

    fn add(&mut self, entry_len: usize) {
        self.count += 1;
        self.len += entry_len;
    }
}

impl<TPrimaryKey, TSlotKey, TItem, TFactory, TReader>
    VariableKeyPreallocatedSlotsArchiveReader<TPrimaryKey, TSlotKey, TItem, TFactory, TReader>
where
    TPrimaryKey: PrimaryKeyVariableSize,
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
        let mut pages: Vec<CompactL1PagePlan> = Vec::new();
        let mut fill = SplitPageFill::new();
        let mut previous: Option<TPrimaryKey> = None;

        for interval in &self.l0 {
            let (frames, entries) = self.source_l1_page(interval).await?;

            for entry in entries {
                refuse_unordered(&previous, &entry.key)?;

                let raw = &frames[entry.frame].body[entry.key_at..entry.key_at + entry.key_len];
                let entry_len = variable_split_l1_entry_len(raw.len());

                if pages.is_empty() || !fill.takes(entry_len) {
                    fill = SplitPageFill::new();
                    pages.push(CompactL1PagePlan {
                        from: raw.to_vec(),
                        to: Vec::new(),
                        count: 0,
                        len: 0,
                    });
                }

                fill.add(entry_len);

                let page = pages.last_mut().unwrap();
                page.to.clear();
                page.to.extend_from_slice(raw);
                page.count = fill.count;
                page.len = fill.len;

                previous = Some(entry.key);
            }
        }

        let l0_len = SPLIT_L0_HEADER_LEN as u64
            + pages
                .iter()
                .map(|page| variable_split_l0_entry_len(page.from.len(), page.to.len()) as u64)
                .sum::<u64>();

        let layout = DataFileLayout::new(
            split_data_page_len::<TSlotKey, TItem>(),
            self.max_data_file_bytes(),
        );

        CompactionPlan::new(pages, l0_len, layout)
    }

    /// Writes the COMPACT archive of `plan` through `out`: each data file in order, append only, each
    /// finished before the next is created; then the index, append only, finished LAST.
    ///
    /// ONE L0 frame of exact size (`next` 0) · L1 pages of exact size, each closed at
    /// [`COMPACT_L1_MAX_KEYS`] keys or before an entry would take it past [`COMPACT_L1_PAGE_BYTES`], with
    /// no end marker and no padding · the data page of the key of rank `i` at `ref(i)` of the plan's
    /// layout, as one frame. Key bytes are copied raw from the source. The bytes are a function of the
    /// keys, the items and the plan's `max_data_file_bytes` only, so a run over the same archive with the
    /// same `max_data_file_bytes` writes identical files.
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
            let (_, entries) = self.source_l1_page(interval).await?;

            for entry in entries {
                let body = self.source_data_body(entry.data).await?;
                data.push(out, &body).await?;
            }
        }

        data.finish().await?;

        let mut index = CompactIndexWriter::create(out, plan, &compact_l0_frame(plan)?).await?;

        let mut page = CompactL1PageBuilder::new();
        let mut previous: Option<TPrimaryKey> = None;
        let mut rank = 0;

        for interval in &self.l0 {
            let (frames, entries) = self.source_l1_page(interval).await?;

            for entry in entries {
                if rank >= plan.keys {
                    return Err(source_changed_while_compacting());
                }

                refuse_unordered(&previous, &entry.key)?;

                let raw = &frames[entry.frame].body[entry.key_at..entry.key_at + entry.key_len];

                if !page.fill.takes(variable_split_l1_entry_len(raw.len())) {
                    page.push_to(&mut index).await?;
                }

                page.add(raw, plan.layout.ref_of(rank)?);
                previous = Some(entry.key);
                rank += 1;
            }
        }

        if page.fill.count > 0 {
            page.push_to(&mut index).await?;
        }
        drop(page);

        index.finish().await
    }
}

/// The compact L1 page being filled: its entries, its fill and its first and last raw key.
struct CompactL1PageBuilder {
    body: Vec<u8>,
    fill: SplitPageFill,
    from: Vec<u8>,
    to: Vec<u8>,
}

impl CompactL1PageBuilder {
    fn new() -> Self {
        Self {
            body: Vec::new(),
            fill: SplitPageFill::new(),
            from: Vec::new(),
            to: Vec::new(),
        }
    }

    fn add(&mut self, raw: &[u8], data: DataRef) {
        if self.fill.count == 0 {
            self.from = raw.to_vec();
        }

        push_variable_split_l1_entry(&mut self.body, raw, data);
        self.fill.add(variable_split_l1_entry_len(raw.len()));
        self.to = raw.to_vec();
    }

    /// Stamps the page at its exact size, hands it to the index — which checks it against the plan — and
    /// starts the next one.
    async fn push_to<TWriter: ArchiveFileWriter>(
        &mut self,
        index: &mut CompactIndexWriter<'_, TWriter>,
    ) -> Result<(), ArchiveError> {
        let bytes = encode_split_l1_frame(self.fill.len, 0, self.fill.count, &self.body)?;
        index
            .push_l1_page(&bytes, self.fill.count, &self.from, &self.to)
            .await?;

        *self = Self::new();
        Ok(())
    }
}

/// The one L0 frame: every planned page's first and last raw key and where it starts.
fn compact_l0_frame(plan: &CompactionPlan) -> Result<Vec<u8>, ArchiveError> {
    let body = encode_variable_split_l0_bodies(
        plan.pages
            .iter()
            .zip(plan.l1_offsets())
            .map(|(page, offset)| (&page.from[..], &page.to[..], offset)),
        usize::MAX,
    )
    .pop();

    let (count, body) = body.map_or((0, Vec::new()), |frame| (frame.count, frame.body));

    encode_split_l0_frame(SPLIT_L0_HEADER_LEN + body.len(), 0, count, &body)
}
