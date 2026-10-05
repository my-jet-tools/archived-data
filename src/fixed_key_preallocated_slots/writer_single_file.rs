use std::collections::HashMap;

use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};

use crate::{
    ArchiveError, CheckedItemBytes, CheckedSlotIndex, DEFAULT_L0_CAPACITY, DEFAULT_L1_CAPACITY,
    DEFAULT_WRITER_L1_CACHE_PAGES, L0Entry, PositionedSink, PreallocatedDataItem,
    PreallocatedSlotKey, PrimaryKeyFixedSize, WorkingL0Chain, WriterDataPages, WriterPage,
    check_type_sizes, decode_l1_page, dirty_offsets, encode_l0_bodies, encode_l1_page,
    l0_frame_len, l1_page_len, least_recently_used,
};

/// Builds the WORKING file from items written in any key order.
///
/// L0 is held in memory and written as a CHAIN of frames of `capacity_l0` intervals each: frame 0 is
/// reserved at offset 0, and at flush every frame beyond it is a continuation of the same size, reused
/// if an earlier flush allocated it and appended at the end of the file if not. L0 has no limit. L1
/// pages and data pages are appended in allocation order. A new key is inserted, sorted, into the L1
/// page whose interval it falls into (a full page is split in half, which adds an L0 interval) and gets
/// a data page preallocated with `TItem::default()` in every slot. A known key has its item written
/// into its slot in place.
///
/// L1 and data pages are kept in bounded LRU caches and reach the sink on eviction or on
/// [`Self::flush`]; an item written to a data page that is not cached goes straight to the sink.
/// **Nothing written is durable until [`Self::flush`] or [`Self::finish`] returns `Ok`** — dropping
/// the writer without one leaves an unreadable file.
///
/// Every `Err` leaves the file in an unknown state: the writer is not usable after it.
pub struct FixedKeyPreallocatedSlotsArchiveWriterSingleFile<TPrimaryKey, TSlotKey, TItem, TSink> {
    sink: PositionedSink<TSink>,
    capacity_l0: usize,
    capacity_l1: usize,
    l0: Vec<L0Entry<TPrimaryKey>>,
    l0_chain: WorkingL0Chain,
    end: u64,
    keys: usize,
    tick: u64,
    l1_cache_size: usize,
    l1_pages: HashMap<u64, WriterPage<Vec<(TPrimaryKey, u64)>>>,
    data_pages: WriterDataPages<TSlotKey, TItem>,
}

impl<TPrimaryKey, TSlotKey, TItem, TSink>
    FixedKeyPreallocatedSlotsArchiveWriterSingleFile<TPrimaryKey, TSlotKey, TItem, TSink>
where
    TPrimaryKey: PrimaryKeyFixedSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    /// [`DEFAULT_L0_CAPACITY`] intervals per L0 frame, [`DEFAULT_L1_CAPACITY`] keys per L1 page.
    /// The sink is expected empty: it is written from offset 0 and never truncated.
    pub fn new(sink: TSink) -> Self {
        Self::new_with_capacity(sink, DEFAULT_L0_CAPACITY, DEFAULT_L1_CAPACITY)
    }

    /// `capacity_l0` is intervals per L0 FRAME, not a limit on L0: more intervals chain more frames.
    ///
    /// Panics when a capacity is zero or a page of these types cannot fit its `u32` size.
    pub fn new_with_capacity(sink: TSink, capacity_l0: usize, capacity_l1: usize) -> Self {
        check_type_sizes::<TPrimaryKey, TSlotKey, TItem>(capacity_l0, capacity_l1);

        let l0_frame = l0_frame_len::<TPrimaryKey>(capacity_l0);

        Self {
            sink: PositionedSink::new(sink),
            capacity_l0,
            capacity_l1,
            l0: Vec::new(),
            l0_chain: WorkingL0Chain::new(l0_frame),
            end: l0_frame as u64,
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

        let data_offset = match self.data_offset_of(&key).await? {
            Some(offset) => offset,
            None => self.insert_key(key).await?,
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

        let bodies = encode_l0_bodies(&self.l0, self.capacity_l0)?;
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
            .partition_point(|entry| entry.from <= *key)
            .saturating_sub(1)
    }

    async fn data_offset_of(&mut self, key: &TPrimaryKey) -> Result<Option<u64>, ArchiveError> {
        if self.l0.is_empty() {
            return Ok(None);
        }

        let interval = self.interval_of(key);
        let l1_offset = self.l0[interval].offset;
        self.load_l1_page(interval).await?;

        let page = &self.l1_pages[&l1_offset].value;

        Ok(page
            .binary_search_by(|(stored, _)| stored.cmp(key))
            .ok()
            .map(|index| page[index].1))
    }

    /// Only called for a key that is not in the file.
    async fn insert_key(&mut self, key: TPrimaryKey) -> Result<u64, ArchiveError> {
        if self.l0.is_empty() {
            let data_offset = self.allocate_data_page().await?;
            let l1_offset = self.allocate(l1_page_len::<TPrimaryKey>(self.capacity_l1));

            self.l0.push(L0Entry {
                from: key.clone(),
                to: key.clone(),
                offset: l1_offset,
            });
            self.cache_l1_page(l1_offset, vec![(key, data_offset)])
                .await?;

            self.keys += 1;
            return Ok(data_offset);
        }

        let interval = self.interval_of(&key);
        let l1_offset = self.l0[interval].offset;
        self.load_l1_page(interval).await?;

        let data_offset = self.allocate_data_page().await?;

        let tick = self.next_tick();
        let page = self.l1_pages.get_mut(&l1_offset).unwrap();
        let at = page
            .value
            .binary_search_by(|(stored, _)| stored.cmp(&key))
            .unwrap_err();
        page.value.insert(at, (key, data_offset));
        page.dirty = true;
        page.last_used = tick;

        let right = if page.value.len() > self.capacity_l1 {
            let half = page.value.len() / 2;
            Some(page.value.split_off(half))
        } else {
            None
        };

        let left = &page.value;
        self.l0[interval].from = left[0].0.clone();
        self.l0[interval].to = left[left.len() - 1].0.clone();

        if let Some(right) = right {
            let right_offset = self.allocate(l1_page_len::<TPrimaryKey>(self.capacity_l1));

            self.l0.insert(
                interval + 1,
                L0Entry {
                    from: right[0].0.clone(),
                    to: right[right.len() - 1].0.clone(),
                    offset: right_offset,
                },
            );
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
        let offset = self.l0[interval].offset;
        let tick = self.next_tick();

        if let Some(page) = self.l1_pages.get_mut(&offset) {
            page.last_used = tick;
            return Ok(());
        }

        let mut bytes = vec![0u8; l1_page_len::<TPrimaryKey>(self.capacity_l1)];
        self.sink.read_at(offset, &mut bytes).await?;
        let entries = decode_l1_page(&bytes, &self.l0[interval])?;

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
        entries: Vec<(TPrimaryKey, u64)>,
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

        let bytes = encode_l1_page(&page.value, self.capacity_l1)?;
        self.sink.write_at(offset, &bytes).await?;
        self.l1_pages.get_mut(&offset).unwrap().dirty = false;
        Ok(())
    }
}
