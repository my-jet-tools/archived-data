use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, CheckedItemBytes, CheckedKeyBytes,
    CheckedSlotIndex, DEFAULT_L0_CAPACITY, DEFAULT_L1_CAPACITY, DEFAULT_MAX_DATA_FILE_BYTES,
    DataFileLayout, DataRef, InFlightGuard, IoReaderFactory, IoWriterFactory, PreallocatedDataItem,
    PreallocatedSlotKey, PrimaryKeyFixedSize, SplitDataWriters, SplitFrame, SplitFrameKind,
    SplitIndexWriter, SplitKeyPlacement, SplitL0Interval, SplitWorkingL0Chain,
    SplitWriterDataPages, SplitWriterL1Pages, check_split_type_sizes, decode_split_l1_chain,
    encode_split_l0_bodies, encode_split_l1_page, finish_split_writers,
    flush_split_working_archive, split_l0_frame_len, split_l1_page_len,
};

/// Builds a WORKING split archive — an index and data files `0 ..= 255` — from items written in any key
/// order, through the writers (and, to read its own evicted L1 pages back, the readers) of one factory.
///
/// The index holds L0 frame 0 at offset 0, appended when the writer is created for `capacity_l0`
/// intervals with a hash that does not verify, then L1 pages of `capacity_l1` keys, and at flush the L0
/// continuations it needs. Data pages go to the current data file while they fit whole in
/// `max_data_file_bytes`, then to the next one. A new key is inserted, sorted, into the L1 page of its
/// interval and gets a data page preallocated with `TItem::default()` in every slot; a known key has its
/// item written into its slot in place, in whichever data file the page lives.
///
/// A page that overflows is not always halved. A key past the last key of the LAST page, when that page is
/// full, starts a new page of its own, so keys written in ascending order fill every page completely. Any
/// other overflow halves the page, and the upper half moves in front of the NEXT page's keys when it fits
/// there whole; only when it does not is a new page allocated.
///
/// Files grow only by append. An offset is handed out ahead of its bytes, and a page past a file's append
/// frontier is always cached, so a page leaving the cache appends every page before it first.
///
/// **Nothing written is readable until [`Self::flush`] or [`Self::finish`] returns `Ok`.** `finish`, and a
/// writer dropped without it, persist what reached each file — which says nothing about the archive being
/// complete (see [`crate::ArchiveFileWriter`]). The index opens only when its last flush completed and nothing
/// was written to its files after it; otherwise `open` is `Corrupted` — an index no flush completed on, one a
/// flush stopped halfway through, and one that a page or an item reached after its last flush completed. A
/// write whose changes all stayed in the caches wrote nothing to the files: it is not in the archive until a
/// flush, and does not stop it from opening.
///
/// A refusal changes nothing, and the writer stays usable after it: a slot or an item out of contract, a
/// new key that does not encode, that needs data file 256, or that needs an L1 page the index has no room
/// left for — room counted with the L0 continuations a flush will need, so a flush never runs into the
/// limit. Every one of them is decided before the key changes anything. Any other `Err`, and a `write` or
/// `flush` future dropped before it returned — neither is cancel-safe — leaves every later `write`,
/// `flush` and `finish` refused as a `ContractViolation`; `finish` still finishes every file.
///
/// A key written before is never lost to a write that fails or is dropped: where a new key goes, and where
/// the upper half of a page it overflows goes, is decided — with the pages involved read into the cache —
/// before the page is cut, and the cut is made with no await in it.
pub struct FixedKeyPreallocatedSlotsArchiveWriter<
    TPrimaryKey,
    TSlotKey,
    TItem,
    TFactory,
    TWriter,
    TReader,
> {
    factory: TFactory,
    index: SplitIndexWriter<TWriter, TReader>,
    data_files: SplitDataWriters<TWriter>,
    capacity_l0: usize,
    capacity_l1: usize,
    l0: Vec<SplitL0Interval<TPrimaryKey>>,
    l0_chain: SplitWorkingL0Chain,
    keys: usize,
    l1_pages: SplitWriterL1Pages<(TPrimaryKey, DataRef)>,
    data_pages: SplitWriterDataPages<TSlotKey, TItem>,
    in_flight: InFlightGuard,
}

impl<TPrimaryKey, TSlotKey, TItem, TFactory, TWriter, TReader>
    FixedKeyPreallocatedSlotsArchiveWriter<TPrimaryKey, TSlotKey, TItem, TFactory, TWriter, TReader>
where
    TPrimaryKey: PrimaryKeyFixedSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TFactory: IoWriterFactory<TWriter> + IoReaderFactory<TReader>,
    TWriter: ArchiveFileWriter,
    TReader: ArchiveFileReader,
{
    /// [`DEFAULT_L0_CAPACITY`] intervals per L0 frame, [`DEFAULT_L1_CAPACITY`] keys per L1 page.
    pub async fn new(factory: TFactory) -> Result<Self, ArchiveError> {
        Self::new_with_capacity(factory, DEFAULT_L0_CAPACITY, DEFAULT_L1_CAPACITY).await
    }

    /// `capacity_l0` is intervals per L0 FRAME, not a limit on L0: more intervals chain more frames.
    /// Creates the index and appends frame 0; data file 0 is created with the first key.
    ///
    /// Panics when a capacity is zero or a page of these types cannot fit its `u32` size.
    pub async fn new_with_capacity(
        factory: TFactory,
        capacity_l0: usize,
        capacity_l1: usize,
    ) -> Result<Self, ArchiveError> {
        check_split_type_sizes::<TPrimaryKey, TSlotKey, TItem>(capacity_l0, capacity_l1);

        let l0_chain = SplitWorkingL0Chain::new(split_l0_frame_len::<TPrimaryKey>(capacity_l0));
        let index = SplitIndexWriter::create(&factory, &l0_chain.unflushed_frame()?).await?;

        let data_pages = SplitWriterDataPages::new();
        let layout = DataFileLayout::new(data_pages.page_len(), DEFAULT_MAX_DATA_FILE_BYTES);

        Ok(Self {
            factory,
            index,
            data_files: SplitDataWriters::new(layout),
            capacity_l0,
            capacity_l1,
            l0: Vec::new(),
            l0_chain,
            keys: 0,
            l1_pages: SplitWriterL1Pages::new(split_l1_page_len::<TPrimaryKey>(capacity_l1)),
            data_pages,
            in_flight: InFlightGuard::new(),
        })
    }

    /// At least 1. A smaller cache only costs IO. A write placing a new key holds up to three pages more
    /// until it returns: the key's page, the next one, and a new one.
    pub fn set_l1_cache_size(&mut self, pages: usize) {
        self.l1_pages.set_cache_size(pages);
    }

    /// How many bytes of data pages a data file takes before the next one starts; 4 GiB by default. A
    /// setting of the archive's shape, so it is made before the first key.
    ///
    /// Panics above 4 GiB, below one data page, or once a key is written.
    pub fn set_max_data_file_bytes(&mut self, bytes: u64) {
        assert!(
            self.keys == 0,
            "the data file size cannot change once a key is written"
        );
        self.data_files
            .set_layout(DataFileLayout::new(self.data_pages.page_len(), bytes));
    }

    pub fn max_data_file_bytes(&self) -> u64 {
        self.data_files.layout().max_data_file_bytes()
    }

    /// 0 appends every new data page at once and writes every item straight to its data file.
    pub fn set_data_cache_size(&mut self, pages: usize) {
        self.data_pages.set_cache_size(pages);
    }

    pub fn keys_count(&self) -> usize {
        self.keys
    }

    pub fn l0_intervals(&self) -> usize {
        self.l0.len()
    }

    /// The L0 frames the index has room for so far: frame 0 plus every continuation a flush reserved.
    pub fn l0_frames(&self) -> usize {
        self.l0_chain.frames()
    }

    /// The data files created so far.
    pub fn data_files(&self) -> usize {
        self.data_files.files()
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

        self.in_flight.begin()?;

        let at = match self.data_ref_of(&key).await? {
            Some(at) => at,
            None => match self.place_new_key(&key).await? {
                Ok(placement) => self.insert_key(key, placement).await?,
                Err(refused) => {
                    self.evict_l1_pages().await?;
                    self.in_flight.complete();
                    return Err(refused);
                }
            },
        };

        self.evict_l1_pages().await?;

        let written = self
            .data_pages
            .write_item(&self.data_files, &mut self.index, at, slot, &item_bytes)
            .await;

        self.in_flight.complete_if_ok(written)
    }

    /// Every cached L1 page reaches the index in offset order, every cached data page its data file, then
    /// the L0 chain is laid out — the continuations it has, new ones appended at the end — and frame 0 is
    /// written LAST. Frame 0's hash is spoiled before the first byte written since the last flush completed —
    /// by this flush, or earlier by a write — so an index whose flush stopped after that first byte reached a
    /// file is refused at open as `Corrupted` (see [`crate::ArchiveFileWriter`]). A flush that stopped before
    /// it changed nothing, and the index opens as the last completed flush left it. With nothing written since
    /// the last flush, a flush writes nothing at all.
    pub async fn flush(&mut self) -> Result<(), ArchiveError> {
        self.in_flight.begin()?;
        let flushed = self.flush_in_flight().await;
        self.in_flight.complete_if_ok(flushed)
    }

    async fn flush_in_flight(&mut self) -> Result<(), ArchiveError> {
        let l0_bodies = encode_split_l0_bodies(&self.l0, self.capacity_l0)?;

        flush_split_working_archive(
            &mut self.index,
            &mut self.l0_chain,
            &mut self.l1_pages,
            &l1_encoder(self.capacity_l1),
            &mut self.data_pages,
            &mut self.data_files,
            l0_bodies,
        )
        .await
    }

    /// [`Self::flush`], then `finish` on the index and on every data file created, each one awaited even
    /// after another failed. The flush's error comes first, then the first failed finish.
    pub async fn finish(mut self) -> Result<(), ArchiveError> {
        let flushed = self.flush().await;
        let finished = finish_split_writers(self.index, self.data_files).await;

        flushed?;
        finished
    }

    /// A limit on the index below 4 GiB, so a test reaches it with a few pages.
    #[cfg(test)]
    pub(crate) fn set_max_index_bytes(&mut self, bytes: u64) {
        self.index.set_max_bytes(bytes);
    }

    /// Forgets that a call never completed, so a test can show what the writer's state was when it stopped.
    #[cfg(test)]
    pub(crate) fn forget_in_flight(&mut self) {
        self.in_flight.complete();
    }

    fn interval_of(&self, key: &TPrimaryKey) -> usize {
        self.l0
            .partition_point(|entry| entry.from <= *key)
            .saturating_sub(1)
    }

    fn l1_offset(&self, interval: usize) -> u64 {
        self.l0[interval].l1_offset as u64
    }

    async fn data_ref_of(&mut self, key: &TPrimaryKey) -> Result<Option<DataRef>, ArchiveError> {
        if self.l0.is_empty() {
            return Ok(None);
        }

        let interval = self.interval_of(key);
        self.load_l1_page(interval).await?;

        let page = self.cached_l1_page(self.l1_offset(interval));

        Ok(page
            .binary_search_by(|(stored, _)| stored.cmp(key))
            .ok()
            .map(|at| page[at].1))
    }

    /// Where a key that is not in the archive goes, decided before anything changes. The inner `Err` is a
    /// refusal that changed nothing, after which the writer stays usable: a key that does not encode, a
    /// key needing data file 256, an index with no room for the L1 page the key needs. The outer one is a
    /// failed read.
    ///
    /// The pages it reads only join the cache — the key's page and, when that is full, the next one — and
    /// nothing is evicted until the key is placed, so both are still cached when the page is cut.
    async fn place_new_key(
        &mut self,
        key: &TPrimaryKey,
    ) -> Result<Result<SplitKeyPlacement, ArchiveError>, ArchiveError> {
        if let Err(refused) = key.copy_key_checked_to(&mut Vec::with_capacity(TPrimaryKey::LEN)) {
            return Ok(Err(refused));
        }

        if let Err(refused) = self.data_files.layout().ref_of(self.keys) {
            return Ok(Err(refused));
        }

        if self.l0.is_empty() {
            return Ok(self
                .check_new_page_room()
                .map(|()| SplitKeyPlacement::NewPage { at: 0 }));
        }

        let interval = self.interval_of(key);
        self.load_l1_page(interval).await?;
        let len = self.cached_l1_page(self.l1_offset(interval)).len();

        if len < self.capacity_l1 {
            return Ok(Ok(SplitKeyPlacement::Insert { interval }));
        }

        let next = interval + 1;

        if next == self.l0.len() && *key > self.l0[interval].to {
            return Ok(self
                .check_new_page_room()
                .map(|()| SplitKeyPlacement::NewPage { at: next }));
        }

        if next < self.l0.len() {
            self.load_l1_page(next).await?;

            // The page is halved after the insert: `len + 1` keys, the lower `(len + 1) / 2` stay.
            let upper_half = (len + 1).div_ceil(2);

            if self.cached_l1_page(self.l1_offset(next)).len() + upper_half <= self.capacity_l1 {
                return Ok(Ok(SplitKeyPlacement::SplitIntoNext { interval }));
            }
        }

        Ok(self
            .check_new_page_room()
            .map(|()| SplitKeyPlacement::SplitIntoNewPage { interval }))
    }

    /// One more L1 page, and the L0 continuations a flush will need for one more interval, must fit the
    /// index — so neither placing the key nor a later flush can run into the index limit halfway.
    fn check_new_page_room(&self) -> Result<(), ArchiveError> {
        let frames = (self.l0.len() + 1).div_ceil(self.capacity_l0);
        let continuations = frames.saturating_sub(self.l0_chain.frames());
        let continuation_len = split_l0_frame_len::<TPrimaryKey>(self.capacity_l0);

        self.index
            .check_reservable(self.l1_pages.page_len() + continuations * continuation_len)
            .map(|_| ())
    }

    /// Places a key where [`Self::place_new_key`] decided. Its data page is preallocated first; after that
    /// the key goes into L1 with no await between the first change and the last, so a failed or dropped
    /// write never leaves an L1 page cut with its upper half nowhere. An L1 page is never cached with more
    /// than `capacity_l1` keys, because an eviction may encode any cached page.
    async fn insert_key(
        &mut self,
        key: TPrimaryKey,
        placement: SplitKeyPlacement,
    ) -> Result<DataRef, ArchiveError> {
        let at = self
            .data_pages
            .preallocate(
                &mut self.data_files,
                &mut self.index,
                &self.factory,
                self.keys,
            )
            .await?;

        let entry = (key, at);

        match placement {
            SplitKeyPlacement::NewPage { at: position } => {
                let offset = self.index.reserve(self.l1_pages.page_len())?;
                self.put_new_page(position, offset, vec![entry]);
            }
            SplitKeyPlacement::Insert { interval } => {
                let upper_half = self.insert_into_page(interval, entry);
                debug_assert!(upper_half.is_empty());
            }
            SplitKeyPlacement::SplitIntoNext { interval } => {
                let upper_half = self.insert_into_page(interval, entry);
                let next = interval + 1;
                let offset = self.l1_offset(next);

                self.l0[next].from = upper_half[0].0.clone();
                self.l1_pages
                    .modify(offset)
                    .expect("a placed key's pages are cached")
                    .splice(0..0, upper_half);
            }
            SplitKeyPlacement::SplitIntoNewPage { interval } => {
                let offset = self.index.reserve(self.l1_pages.page_len())?;
                let upper_half = self.insert_into_page(interval, entry);
                self.put_new_page(interval + 1, offset, upper_half);
            }
        }

        self.keys += 1;
        Ok(at)
    }

    /// Inserts `entry` into the page of `interval`, halves the page if it overflows, and narrows the
    /// interval to what stays. Returns the upper half, empty when the page did not overflow.
    fn insert_into_page(
        &mut self,
        interval: usize,
        entry: (TPrimaryKey, DataRef),
    ) -> Vec<(TPrimaryKey, DataRef)> {
        let page = self
            .l1_pages
            .modify(self.l0[interval].l1_offset as u64)
            .expect("a placed key's pages are cached");

        let position = page
            .binary_search_by(|(stored, _)| stored.cmp(&entry.0))
            .unwrap_err();
        page.insert(position, entry);

        let upper_half = if page.len() > self.capacity_l1 {
            page.split_off(page.len() / 2)
        } else {
            Vec::new()
        };

        self.l0[interval].from = page[0].0.clone();
        self.l0[interval].to = page[page.len() - 1].0.clone();

        upper_half
    }

    /// A new L1 page holding `entries` at the reserved `offset`, with its interval at L0 position `at`.
    fn put_new_page(&mut self, at: usize, offset: u32, entries: Vec<(TPrimaryKey, DataRef)>) {
        self.l0.insert(
            at,
            SplitL0Interval {
                from: entries[0].0.clone(),
                to: entries[entries.len() - 1].0.clone(),
                l1_offset: offset,
            },
        );

        self.l1_pages.insert(offset as u64, entries, true);
    }

    /// Caches the page of `interval`, evicting nothing. A page that is not cached is appended already — a
    /// page past the frontier is always cached — so it is read back through a reader of the index.
    async fn load_l1_page(&mut self, interval: usize) -> Result<(), ArchiveError> {
        let offset = self.l0[interval].l1_offset;

        if self.l1_pages.touch(offset as u64) {
            return Ok(());
        }

        let len = self.l1_pages.page_len();
        let bytes = self.index.read_back(&self.factory, offset, len).await?;
        let frame = SplitFrame::from_bytes(bytes, offset as u64, SplitFrameKind::L1)?;
        let entries = decode_split_l1_chain(&[frame], &self.l0[interval])?;

        self.l1_pages.insert(offset as u64, entries, false);

        Ok(())
    }

    /// Evicts down to the cache size, once a write no longer needs the pages it read in.
    async fn evict_l1_pages(&mut self) -> Result<(), ArchiveError> {
        let keep = self.l1_pages.cache_size();

        self.l1_pages
            .evict(&mut self.index, keep, &l1_encoder(self.capacity_l1))
            .await
    }

    fn cached_l1_page(&self, offset: u64) -> &[(TPrimaryKey, DataRef)] {
        self.l1_pages
            .get(offset)
            .expect("an L1 page just loaded is cached")
    }
}

fn l1_encoder<TKey: PrimaryKeyFixedSize>(
    capacity: usize,
) -> impl Fn(&[(TKey, DataRef)]) -> Result<Vec<u8>, ArchiveError> {
    move |entries| encode_split_l1_page(entries, capacity)
}
