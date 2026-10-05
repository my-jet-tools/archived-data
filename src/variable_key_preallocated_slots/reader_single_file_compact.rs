use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, AsyncWriteExt};

use crate::{
    ArchiveError, COMPACT_L1_MAX_KEYS, COMPACT_L1_PAGE_BYTES, L0_HEADER_LEN, PreallocatedDataItem,
    PreallocatedSlotKey, PrimaryKeyVariableSize, VARIABLE_L1_HEADER_LEN,
    VariableKeyPreallocatedSlotsArchiveReaderSingleFile, begin_variable_l1_page, data_page_len,
    decode_variable_l1_page, encode_l0_frame, encode_variable_l0_bodies, finish_variable_l1_page,
    push_variable_l1_entry, variable_l0_entry_len, variable_l1_entry_len,
};

/// How full a compact L1 page is, header included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageFill {
    count: usize,
    len: usize,
}

impl PageFill {
    fn new() -> Self {
        Self {
            count: 0,
            len: VARIABLE_L1_HEADER_LEN,
        }
    }

    /// A page closes at [`COMPACT_L1_MAX_KEYS`] keys, or before an entry that would take it past
    /// [`COMPACT_L1_PAGE_BYTES`]. An empty page takes any entry: a working file written with larger L1
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

/// One L1 page of the compact file, as pass 1 plans it: the raw bytes of its bounds and its fill.
struct CompactL1Page {
    from: Vec<u8>,
    to: Vec<u8>,
    fill: PageFill,
}

struct CompactPlan {
    pages: Vec<CompactL1Page>,
    keys: usize,
    l0_len: usize,
}

impl CompactPlan {
    fn l1_len(&self) -> usize {
        self.pages.iter().map(|page| page.fill.len).sum()
    }

    fn total_len<TSlotKey: PreallocatedSlotKey, TItem: PreallocatedDataItem>(&self) -> u64 {
        (self.l0_len + self.l1_len()) as u64
            + self.keys as u64 * data_page_len::<TSlotKey, TItem>() as u64
    }
}

impl<TPrimaryKey, TSlotKey, TItem, TSource>
    VariableKeyPreallocatedSlotsArchiveReaderSingleFile<TPrimaryKey, TSlotKey, TItem, TSource>
where
    TPrimaryKey: PrimaryKeyVariableSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TSource: AsyncRead + AsyncSeek + Unpin,
{
    /// The exact length [`Self::write_compacted_to`] writes. Keys vary in size, so this is a pass over
    /// every L1 page of the source.
    pub async fn compacted_len(&self) -> Result<u64, ArchiveError> {
        Ok(self.compact_plan().await?.total_len::<TSlotKey, TItem>())
    }

    /// Writes the COMPACT file to `out`, from its first byte, and returns the length written:
    ///
    /// ONE L0 frame of exact size (`next` 0) · L1 pages of exact size, each closed at
    /// [`COMPACT_L1_MAX_KEYS`] keys or before an entry would take it past [`COMPACT_L1_PAGE_BYTES`],
    /// with no end marker and no padding · every data page copied whole in ascending key order · all
    /// offsets absolute. Key bytes are copied raw from the source. The bytes are a function of the keys
    /// and the items only.
    ///
    /// Three passes over the source's L1 pages (plan, L1, data), none through the caches; memory is L0
    /// plus one L1 page plus one data page. `out` is neither flushed nor shut down.
    pub async fn write_compacted_to<TOut: AsyncWrite + Unpin>(
        &self,
        out: &mut TOut,
    ) -> Result<u64, ArchiveError> {
        let plan = self.compact_plan().await?;
        let data_len = data_page_len::<TSlotKey, TItem>() as u64;

        let mut l1_offsets = Vec::with_capacity(plan.pages.len());
        let mut at = plan.l0_len as u64;
        for page in &plan.pages {
            l1_offsets.push(at);
            at += page.fill.len as u64;
        }
        let data_start = at;

        let body = encode_variable_l0_bodies(
            plan.pages
                .iter()
                .zip(&l1_offsets)
                .map(|(page, offset)| (&page.from[..], &page.to[..], *offset)),
            usize::MAX,
        )
        .pop();
        let (count, body) = body.map_or((0, Vec::new()), |frame| (frame.count, frame.body));

        let frame = encode_l0_frame(L0_HEADER_LEN + body.len(), 0, count, &body)?;
        out.write_all(&frame).await?;
        let mut written = frame.len() as u64;
        drop(l1_offsets);

        let mut page = begin_variable_l1_page();
        let mut fill = PageFill::new();
        let mut index = 0;
        let mut rank = 0u64;

        for interval in &self.l0 {
            let bytes = self.read_l1_bytes(interval.offset).await?;

            for entry in decode_variable_l1_page(&bytes, interval)? {
                let raw = &bytes[entry.key_at..entry.key_at + entry.key_len];
                let entry_len = variable_l1_entry_len(raw.len());

                if !fill.takes(entry_len) {
                    written += write_l1(out, &mut page, fill, plan.pages.get(index)).await?;
                    index += 1;
                    fill = PageFill::new();
                }

                push_variable_l1_entry(&mut page, raw, data_start + rank * data_len);
                fill.add(entry_len);
                rank += 1;
            }
        }

        if fill.count > 0 {
            written += write_l1(out, &mut page, fill, plan.pages.get(index)).await?;
            index += 1;
        }

        if index != plan.pages.len() || rank as usize != plan.keys {
            return Err(changed_underneath());
        }

        for interval in &self.l0 {
            let bytes = self.read_l1_bytes(interval.offset).await?;

            for entry in decode_variable_l1_page(&bytes, interval)? {
                let data = self.read_data_bytes(entry.offset).await?;
                out.write_all(&data).await?;
                written += data.len() as u64;
            }
        }

        if written != plan.total_len::<TSlotKey, TItem>() {
            return Err(changed_underneath());
        }

        Ok(written)
    }

    /// Pass 1: the compact L1 pages, checking that keys ascend across source pages.
    async fn compact_plan(&self) -> Result<CompactPlan, ArchiveError> {
        let mut pages: Vec<CompactL1Page> = Vec::new();
        let mut previous: Option<TPrimaryKey> = None;
        let mut keys = 0;

        for interval in &self.l0 {
            let bytes = self.read_l1_bytes(interval.offset).await?;

            for entry in decode_variable_l1_page(&bytes, interval)? {
                if previous
                    .as_ref()
                    .is_some_and(|previous| *previous >= entry.key)
                {
                    return Err(ArchiveError::Corrupted(
                        "keys do not ascend across L1 pages".to_string(),
                    ));
                }

                let raw = &bytes[entry.key_at..entry.key_at + entry.key_len];
                let entry_len = variable_l1_entry_len(raw.len());

                match pages.last_mut() {
                    Some(page) if page.fill.takes(entry_len) => {
                        page.fill.add(entry_len);
                        page.to.clear();
                        page.to.extend_from_slice(raw);
                    }
                    _ => {
                        let mut fill = PageFill::new();
                        fill.add(entry_len);
                        pages.push(CompactL1Page {
                            from: raw.to_vec(),
                            to: raw.to_vec(),
                            fill,
                        });
                    }
                }

                keys += 1;
                previous = Some(entry.key);
            }
        }

        let l0_len = L0_HEADER_LEN
            + pages
                .iter()
                .map(|page| variable_l0_entry_len(page.from.len(), page.to.len()))
                .sum::<usize>();

        Ok(CompactPlan {
            pages,
            keys,
            l0_len,
        })
    }
}

/// Stamps and writes one compact L1 page, which must be the one pass 1 planned, and starts the next.
async fn write_l1<TOut: AsyncWrite + Unpin>(
    out: &mut TOut,
    page: &mut Vec<u8>,
    fill: PageFill,
    planned: Option<&CompactL1Page>,
) -> Result<u64, ArchiveError> {
    if planned.is_none_or(|planned| planned.fill != fill) {
        return Err(changed_underneath());
    }

    finish_variable_l1_page(page, fill.count, fill.len)?;
    out.write_all(page).await?;

    let len = page.len() as u64;
    *page = begin_variable_l1_page();
    Ok(len)
}

fn changed_underneath() -> ArchiveError {
    ArchiveError::Corrupted("the source changed while it was being compacted".to_string())
}
