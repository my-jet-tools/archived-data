use std::collections::HashMap;

use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};

use crate::{
    ArchiveError, CheckedItemBytes, CheckedSlotIndex, CheckedVariableKeyBytes,
    DEFAULT_L0_FRAME_BYTES, DEFAULT_L1_PAGE_BYTES, DEFAULT_WRITER_L1_CACHE_PAGES, L0_HEADER_LEN,
    PositionedSink, PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyVariableSize,
    VARIABLE_L1_HEADER_LEN, VariableL0Interval, WorkingL0Chain, WriterDataPages, WriterPage,
    begin_variable_l1_page, check_variable_sizes, decode_variable_l1_page, dirty_offsets,
    encode_variable_l0_bodies, finish_variable_l1_page, least_recently_used,
    push_variable_l1_entry, refuse_oversized_key, variable_l1_entry_len,
};

struct WriterL1Entry<TKey> {
    key: TKey,
    bytes: Vec<u8>,
    data_offset: u64,
}

/// An interval, and the raw bytes of its bounds so L0 is encoded without asking the keys again.
struct WriterL0Entry<TKey> {
    interval: VariableL0Interval<TKey>,
    from_bytes: Vec<u8>,
    to_bytes: Vec<u8>,
}

impl<TKey: Clone> WriterL0Entry<TKey> {
    fn spanning(page: &[WriterL1Entry<TKey>], offset: u64) -> Self {
        let first = &page[0];
        let last = &page[page.len() - 1];

        Self {
            interval: VariableL0Interval {
                from: first.key.clone(),
                to: last.key.clone(),
                offset,
            },
            from_bytes: first.bytes.clone(),
            to_bytes: last.bytes.clone(),
        }
    }
}

/// Builds the WORKING file from items written in any key order.
///
/// L0 is held in memory and written as a CHAIN of frames of `l0_frame_bytes` each: frame 0 is reserved
/// at offset 0, and at flush every frame beyond it is a continuation of the same size, reused if an
/// earlier flush allocated it and appended at the end of the file if not. L1 pages are `l1_page_bytes`
/// each and, with data pages, are appended in allocation order. A new key is inserted, sorted, into the
/// L1 page whose interval it falls into; a page its entries no longer fit is SPLIT by bytes — the left
/// page keeps entries in key order while the next one still fits, the rest moves to a new page at the
/// end, and an L0 interval is inserted — and the key gets a data page preallocated with
/// `TItem::default()` in every slot. A known key has its item written into its slot in place.
///
/// A key whose L1 entry (prefix + key + 8) is more than half of what a page holds
/// (`2 × entry > l1_page_bytes − 8`) is refused as a `ContractViolation` before anything is written:
/// that bound is what makes every split produce exactly two pages that fit.
///
/// L1 and data pages are kept in bounded LRU caches and reach the sink on eviction or on
/// [`Self::flush`]; an item written to a data page that is not cached goes straight to the sink.
/// **Nothing written is durable until [`Self::flush`] or [`Self::finish`] returns `Ok`** — dropping
/// the writer without one leaves an unreadable file.
///
/// Every `Err` other than a refused key or slot leaves the file in an unknown state: the writer is not
/// usable after it.
pub struct VariableKeyPreallocatedSlotsArchiveWriterSingleFile<TPrimaryKey, TSlotKey, TItem, TSink>
{
    sink: PositionedSink<TSink>,
    l0_frame_bytes: usize,
    l1_page_bytes: usize,
    l0: Vec<WriterL0Entry<TPrimaryKey>>,
    l0_chain: WorkingL0Chain,
    end: u64,
    keys: usize,
    tick: u64,
    l1_cache_size: usize,
    l1_pages: HashMap<u64, WriterPage<Vec<WriterL1Entry<TPrimaryKey>>>>,
    data_pages: WriterDataPages<TSlotKey, TItem>,
}

impl<TPrimaryKey, TSlotKey, TItem, TSink>
    VariableKeyPreallocatedSlotsArchiveWriterSingleFile<TPrimaryKey, TSlotKey, TItem, TSink>
where
    TPrimaryKey: PrimaryKeyVariableSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    /// [`DEFAULT_L0_FRAME_BYTES`] per L0 frame, [`DEFAULT_L1_PAGE_BYTES`] per L1 page. The sink is
    /// expected empty: it is written from offset 0 and never truncated.
    pub fn new(sink: TSink) -> Self {
        Self::new_with_capacity(sink, DEFAULT_L0_FRAME_BYTES, DEFAULT_L1_PAGE_BYTES)
    }

    /// Both sizes in BYTES, headers included. `l0_frame_bytes` is per L0 FRAME, not a limit on L0.
    ///
    /// Panics when an L1 page cannot hold two entries of a one-byte key, when an L0 frame cannot hold
    /// one interval of the largest key such a page accepts (`16 + 2 × min((l1_page_bytes − 8) / 2 − 8,
    /// 2 + 32767) + 8`), or when a size does not fit `u32`.
    pub fn new_with_capacity(sink: TSink, l0_frame_bytes: usize, l1_page_bytes: usize) -> Self {
        check_variable_sizes::<TSlotKey, TItem>(l0_frame_bytes, l1_page_bytes);

        Self {
            sink: PositionedSink::new(sink),
            l0_frame_bytes,
            l1_page_bytes,
            l0: Vec::new(),
            l0_chain: WorkingL0Chain::new(l0_frame_bytes),
            end: l0_frame_bytes as u64,
            keys: 0,
            tick: 0,
            l1_cache_size: DEFAULT_WRITER_L1_CACHE_PAGES,
            l1_pages: HashMap::new(),
            data_pages: WriterDataPages::new(),
        }
    }

    /// At least 1. A smaller cache only costs IO.
    pub fn set_l1_cache_size(&mut self, pages: usize) {
        self.l1_cache_size = pages.max(1);
    }

    /// 0 writes every item straight to the sink.
    pub fn set_data_cache_size(&mut self, pages: usize) {
        self.data_pages.set_cache_size(pages);
    }

    pub fn keys_count(&self) -> usize {
        self.keys
    }

    pub fn l0_intervals(&self) -> usize {
        self.l0.len()
    }

    /// The L0 frames the file has room for so far: frame 0 plus every continuation a flush allocated.
    /// A flush may link fewer, when the bounds of the intervals got shorter.
    pub fn l0_frames(&self) -> usize {
        self.l0_chain.frames()
    }

    pub async fn write(
        &mut self,
        key: TPrimaryKey,
        slot: TSlotKey,
        item: &TItem,
    ) -> Result<(), ArchiveError> {
        let slot = slot.checked_slot_index()?;

        let mut item_bytes = Vec::with_capacity(TItem::ITEM_LEN);
        item.copy_item_checked_to(&mut item_bytes)?;

        let mut key_bytes = Vec::new();
        key.copy_variable_key_checked_to(&mut key_bytes)?;
        refuse_oversized_key(key_bytes.len(), self.l1_page_bytes)?;

        let data_offset = match self.data_offset_of(&key).await? {
            Some(offset) => offset,
            None => self.insert_key(key, key_bytes).await?,
        };

        self.data_pages
            .write_item(&mut self.sink, data_offset, slot, &item_bytes)
            .await
    }

    /// Everything cached reaches the sink, then the L0 chain — frame 0, then the continuations in chain
    /// order, a missing one allocated at the end of the file first — then the sink is flushed.
    pub async fn flush(&mut self) -> Result<(), ArchiveError> {
        for offset in dirty_offsets(&self.l1_pages) {
            self.store_l1_page(offset).await?;
        }

        self.data_pages.flush(&mut self.sink).await?;

        let bodies = encode_variable_l0_bodies(
            self.l0.iter().map(|entry| {
                (
                    &entry.from_bytes[..],
                    &entry.to_bytes[..],
                    entry.interval.offset,
                )
            }),
            self.l0_frame_bytes - L0_HEADER_LEN,
        );

        let mut end = self.end;
        let frames = self.l0_chain.lay_out(bodies, |len| {
            let offset = end;
            end += len as u64;
            offset
        })?;
        self.end = end;

        for (offset, bytes) in frames {
            self.sink.write_at(offset, &bytes).await?;
        }

        self.sink.flush().await
    }

    /// [`Self::flush`], and the sink back.
    pub async fn finish(mut self) -> Result<TSink, ArchiveError> {
        self.flush().await?;
        Ok(self.sink.into_inner())
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn interval_of(&self, key: &TPrimaryKey) -> usize {
        self.l0
            .partition_point(|entry| entry.interval.from <= *key)
            .saturating_sub(1)
    }

    async fn data_offset_of(&mut self, key: &TPrimaryKey) -> Result<Option<u64>, ArchiveError> {
        if self.l0.is_empty() {
            return Ok(None);
        }

        let interval = self.interval_of(key);
        let l1_offset = self.l0[interval].interval.offset;
        self.load_l1_page(interval).await?;

        let page = &self.l1_pages[&l1_offset].value;

        Ok(page
            .binary_search_by(|stored| stored.key.cmp(key))
            .ok()
            .map(|index| page[index].data_offset))
    }

    /// Only called for a key that is not in the file.
    async fn insert_key(
        &mut self,
        key: TPrimaryKey,
        key_bytes: Vec<u8>,
    ) -> Result<u64, ArchiveError> {
        if self.l0.is_empty() {
            let data_offset = self.allocate_data_page().await?;
            let l1_offset = self.allocate(self.l1_page_bytes);
            let page = vec![WriterL1Entry {
                key,
                bytes: key_bytes,
                data_offset,
            }];

            self.l0.push(WriterL0Entry::spanning(&page, l1_offset));
            self.cache_l1_page(l1_offset, page).await?;

            self.keys += 1;
            return Ok(data_offset);
        }

        let interval = self.interval_of(&key);
        let l1_offset = self.l0[interval].interval.offset;
        self.load_l1_page(interval).await?;

        let data_offset = self.allocate_data_page().await?;

        let tick = self.next_tick();
        let room = self.l1_page_bytes - VARIABLE_L1_HEADER_LEN;
        let page = self.l1_pages.get_mut(&l1_offset).unwrap();
        let at = page
            .value
            .binary_search_by(|stored| stored.key.cmp(&key))
            .unwrap_err();
        page.value.insert(
            at,
            WriterL1Entry {
                key,
                bytes: key_bytes,
                data_offset,
            },
        );
        page.dirty = true;
        page.last_used = tick;

        let right = split_off_overflow(&mut page.value, room);
        self.l0[interval] = WriterL0Entry::spanning(&page.value, l1_offset);

        if let Some(right) = right {
            let right_offset = self.allocate(self.l1_page_bytes);
            self.l0
                .insert(interval + 1, WriterL0Entry::spanning(&right, right_offset));
            self.cache_l1_page(right_offset, right).await?;
        }

        self.keys += 1;
        Ok(data_offset)
    }

    fn allocate(&mut self, len: usize) -> u64 {
        let offset = self.end;
        self.end += len as u64;
        offset
    }

    async fn allocate_data_page(&mut self) -> Result<u64, ArchiveError> {
        let offset = self.allocate(self.data_pages.page_len());
        self.data_pages.preallocate(&mut self.sink, offset).await?;
        Ok(offset)
    }

    async fn load_l1_page(&mut self, interval: usize) -> Result<(), ArchiveError> {
        let offset = self.l0[interval].interval.offset;
        let tick = self.next_tick();

        if let Some(page) = self.l1_pages.get_mut(&offset) {
            page.last_used = tick;
            return Ok(());
        }

        let mut bytes = vec![0u8; self.l1_page_bytes];
        self.sink.read_at(offset, &mut bytes).await?;
        let entries = decode_variable_l1_page(&bytes, &self.l0[interval].interval)?
            .into_iter()
            .map(|entry| WriterL1Entry {
                bytes: bytes[entry.key_at..entry.key_at + entry.key_len].to_vec(),
                key: entry.key,
                data_offset: entry.offset,
            })
            .collect();

        self.evict_l1_pages(self.l1_cache_size - 1).await?;
        self.l1_pages.insert(
            offset,
            WriterPage {
                value: entries,
                dirty: false,
                last_used: tick,
            },
        );

        Ok(())
    }

    async fn cache_l1_page(
        &mut self,
        offset: u64,
        entries: Vec<WriterL1Entry<TPrimaryKey>>,
    ) -> Result<(), ArchiveError> {
        self.evict_l1_pages(self.l1_cache_size - 1).await?;

        let tick = self.next_tick();
        self.l1_pages.insert(
            offset,
            WriterPage {
                value: entries,
                dirty: true,
                last_used: tick,
            },
        );

        Ok(())
    }

    async fn evict_l1_pages(&mut self, keep: usize) -> Result<(), ArchiveError> {
        while self.l1_pages.len() > keep {
            let Some(oldest) = least_recently_used(&self.l1_pages) else {
                break;
            };

            self.store_l1_page(oldest).await?;
            self.l1_pages.remove(&oldest);
        }

        Ok(())
    }

    async fn store_l1_page(&mut self, offset: u64) -> Result<(), ArchiveError> {
        let page = &self.l1_pages[&offset];

        if !page.dirty {
            return Ok(());
        }

        let mut bytes = begin_variable_l1_page();
        for entry in &page.value {
            push_variable_l1_entry(&mut bytes, &entry.bytes, entry.data_offset);
        }
        finish_variable_l1_page(&mut bytes, page.value.len(), self.l1_page_bytes)?;

        self.sink.write_at(offset, &bytes).await?;
        self.l1_pages.get_mut(&offset).unwrap().dirty = false;
        Ok(())
    }
}

/// The entries past the first one that no longer fits `room` bytes, if any. With every entry at most
/// half of `room`, the left part keeps more than half and the right part fits a page of its own.
fn split_off_overflow<TKey>(
    entries: &mut Vec<WriterL1Entry<TKey>>,
    room: usize,
) -> Option<Vec<WriterL1Entry<TKey>>> {
    let mut filled = 0;

    for (index, entry) in entries.iter().enumerate() {
        filled += variable_l1_entry_len(entry.bytes.len());

        if filled > room {
            return Some(entries.split_off(index));
        }
    }

    None
}
