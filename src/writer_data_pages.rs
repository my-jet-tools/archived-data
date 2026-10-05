use std::collections::HashMap;
use std::marker::PhantomData;

use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};

use crate::{
    ArchiveError, DEFAULT_WRITER_DATA_CACHE_PAGES, PositionedSink, PreallocatedDataItem,
    PreallocatedSlotKey, WriterPage, data_page_len, dirty_offsets, empty_data_page,
    least_recently_used, slot_at,
};

/// A writer's data pages: preallocated with `TItem::default()`, cached up to `cache_size`, reaching the
/// sink on eviction or on [`Self::flush`]. An item for a page that is not cached goes straight to the
/// sink.
pub(crate) struct WriterDataPages<TSlotKey, TItem> {
    cache_size: usize,
    pages: HashMap<u64, WriterPage<Vec<u8>>>,
    empty: Option<Vec<u8>>,
    tick: u64,
    _types: PhantomData<fn() -> (TSlotKey, TItem)>,
}

impl<TSlotKey: PreallocatedSlotKey, TItem: PreallocatedDataItem> WriterDataPages<TSlotKey, TItem> {
    pub fn new() -> Self {
        Self {
            cache_size: DEFAULT_WRITER_DATA_CACHE_PAGES,
            pages: HashMap::new(),
            empty: None,
            tick: 0,
            _types: PhantomData,
        }
    }

    /// 0 writes every item straight to the sink.
    pub fn set_cache_size(&mut self, pages: usize) {
        self.cache_size = pages;
    }

    pub fn page_len(&self) -> usize {
        data_page_len::<TSlotKey, TItem>()
    }

    /// A new page at `offset`, which the caller has already allocated [`Self::page_len`] bytes for.
    pub async fn preallocate<TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        sink: &mut PositionedSink<TSink>,
        offset: u64,
    ) -> Result<(), ArchiveError> {
        if self.empty.is_none() {
            self.empty = Some(empty_data_page::<TSlotKey, TItem>()?);
        }

        let page = self.empty.clone().unwrap();

        if self.cache_size == 0 {
            return sink.write_at(offset, &page).await;
        }

        self.evict(sink, self.cache_size - 1).await?;

        let tick = self.next_tick();
        self.pages.insert(
            offset,
            WriterPage {
                value: page,
                dirty: true,
                last_used: tick,
            },
        );

        Ok(())
    }

    pub async fn write_item<TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        sink: &mut PositionedSink<TSink>,
        data_offset: u64,
        slot: usize,
        item: &[u8],
    ) -> Result<(), ArchiveError> {
        let at = slot_at::<TItem>(slot);
        let tick = self.next_tick();

        if let Some(page) = self.pages.get_mut(&data_offset) {
            page.value[at..at + item.len()].copy_from_slice(item);
            page.dirty = true;
            page.last_used = tick;
            return Ok(());
        }

        sink.write_at(data_offset + at as u64, item).await
    }

    pub async fn flush<TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        sink: &mut PositionedSink<TSink>,
    ) -> Result<(), ArchiveError> {
        for offset in dirty_offsets(&self.pages) {
            self.store(sink, offset).await?;
        }

        Ok(())
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    async fn evict<TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        sink: &mut PositionedSink<TSink>,
        keep: usize,
    ) -> Result<(), ArchiveError> {
        while self.pages.len() > keep {
            let Some(oldest) = least_recently_used(&self.pages) else {
                break;
            };

            self.store(sink, oldest).await?;
            self.pages.remove(&oldest);
        }

        Ok(())
    }

    async fn store<TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        sink: &mut PositionedSink<TSink>,
        offset: u64,
    ) -> Result<(), ArchiveError> {
        let Some(page) = self.pages.get_mut(&offset) else {
            return Ok(());
        };

        if !page.dirty {
            return Ok(());
        }

        page.dirty = false;
        sink.write_at(offset, &page.value).await
    }
}
