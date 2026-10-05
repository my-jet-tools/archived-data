use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, CheckedKeyBytes,
    FixedKeyMinuteIndexedRecordsArchiveWriter, IoReaderFactory, IoWriterFactory, L2_FRAME_LEN,
    MinuteCells, PrimaryKeyFixedSize, RECORD_LEN_PREFIX, RecordRef, SPLIT_DATA_HEADER_LEN,
    SplitFrame, SplitFrameKind, SplitKeyPlacement, SplitL0Interval, decode_records_l1_chain,
    split_l0_frame_len,
};

/// The keys of a records writer: where a key's L2 frame is, and where a key not in the archive yet goes.
/// The L1 pages split by the rules of the split working archive — a key past the last key of the last full
/// page starts a new page, any other overflow halves the page and moves its upper half in front of the next
/// page's keys when it fits there — with the offset of an L2 frame where a data page's reference would be.
impl<TKey, TFactory, TWriter, TReader>
    FixedKeyMinuteIndexedRecordsArchiveWriter<TKey, TFactory, TWriter, TReader>
where
    TKey: PrimaryKeyFixedSize,
    TFactory: IoWriterFactory<TWriter> + IoReaderFactory<TReader>,
    TWriter: ArchiveFileWriter,
    TReader: ArchiveFileReader,
{
    /// The offset of the L2 frame of `key`; `None` when the archive does not hold the key.
    pub(crate) async fn l2_offset_of(&mut self, key: &TKey) -> Result<Option<u32>, ArchiveError> {
        if self.l0.is_empty() {
            return Ok(None);
        }

        let interval = self.interval_of(key);
        self.load_l1_page(interval).await?;

        let page = self.cached_l1_page(self.l0[interval].l1_offset);

        Ok(page
            .binary_search_by(|(stored, _)| stored.cmp(key))
            .ok()
            .map(|at| page[at].1))
    }

    /// Where a key that is not in the archive goes, decided before anything changes. The inner `Err` is a
    /// refusal that changed nothing: a key that does not encode, or an index with no room for the key's L2
    /// frame and the L1 page and L0 continuations it may need. The outer one is a failed read.
    pub(crate) async fn place_new_key(
        &mut self,
        key: &TKey,
    ) -> Result<Result<SplitKeyPlacement, ArchiveError>, ArchiveError> {
        if let Err(refused) = key.copy_key_checked_to(&mut Vec::with_capacity(TKey::LEN)) {
            return Ok(Err(refused));
        }

        if self.l0.is_empty() {
            return Ok(self
                .check_index_room(true)
                .map(|()| SplitKeyPlacement::NewPage { at: 0 }));
        }

        let interval = self.interval_of(key);
        self.load_l1_page(interval).await?;
        let len = self.cached_l1_page(self.l0[interval].l1_offset).len();

        if len < self.capacity_l1 {
            return Ok(self
                .check_index_room(false)
                .map(|()| SplitKeyPlacement::Insert { interval }));
        }

        let next = interval + 1;

        if next == self.l0.len() && *key > self.l0[interval].to {
            return Ok(self
                .check_index_room(true)
                .map(|()| SplitKeyPlacement::NewPage { at: next }));
        }

        if next < self.l0.len() {
            self.load_l1_page(next).await?;

            // The page is halved after the insert: `len + 1` keys, the lower `(len + 1) / 2` stay.
            let upper_half = (len + 1).div_ceil(2);

            if self.cached_l1_page(self.l0[next].l1_offset).len() + upper_half <= self.capacity_l1 {
                return Ok(self
                    .check_index_room(false)
                    .map(|()| SplitKeyPlacement::SplitIntoNext { interval }));
            }
        }

        Ok(self
            .check_index_room(true)
            .map(|()| SplitKeyPlacement::SplitIntoNewPage { interval }))
    }

    /// The first record of a key not in the archive yet, placed where [`Self::place_new_key`] decided: an L2
    /// frame with every cell empty but its minute's, a data frame holding the record, and the key with the
    /// frame's offset in L1. After the data file is created, nothing awaits until the key is in L1.
    ///
    /// Panics when the data frame would take the data file past its limit.
    pub(crate) async fn insert_first_record(
        &mut self,
        key: TKey,
        placement: SplitKeyPlacement,
        minute: usize,
        record: &[u8],
    ) -> Result<(), ArchiveError> {
        self.check_data_room(1);
        self.data.create(&self.factory).await?;

        let l2_offset = self.index.reserve(L2_FRAME_LEN)?;
        let frame = self.data.allocate();

        let mut prefixed = Vec::with_capacity(RECORD_LEN_PREFIX + record.len());
        prefixed.extend_from_slice(&(record.len() as u16).to_le_bytes());
        prefixed.extend_from_slice(record);
        self.data
            .modify(frame)
            .insert(SPLIT_DATA_HEADER_LEN, &prefixed);

        let mut cells = MinuteCells::empty();
        cells.set(minute, RecordRef::new(0, frame, SPLIT_DATA_HEADER_LEN));
        self.index_pages.insert_l2(l2_offset, cells, true);

        let entry = (key, l2_offset);

        match placement {
            SplitKeyPlacement::NewPage { at } => {
                let offset = self.index.reserve(self.l1_page_len())?;
                self.put_new_page(at, offset, vec![entry]);
            }
            SplitKeyPlacement::Insert { interval } => {
                let upper_half = self.insert_into_page(interval, entry);
                debug_assert!(upper_half.is_empty());
            }
            SplitKeyPlacement::SplitIntoNext { interval } => {
                let upper_half = self.insert_into_page(interval, entry);
                let next = interval + 1;
                let offset = self.l0[next].l1_offset;

                self.l0[next].from = upper_half[0].0.clone();
                self.index_pages
                    .modify_l1(offset)
                    .expect("a placed key's pages are cached")
                    .splice(0..0, upper_half);
            }
            SplitKeyPlacement::SplitIntoNewPage { interval } => {
                let offset = self.index.reserve(self.l1_page_len())?;
                let upper_half = self.insert_into_page(interval, entry);
                self.put_new_page(interval + 1, offset, upper_half);
            }
        }

        self.keys += 1;
        Ok(())
    }

    /// The key's L2 frame, one more L1 page with `new_page`, and the L0 continuations a flush will need for
    /// L0 as it will be must all fit the index — so neither placing the key nor a later flush can run into the
    /// index limit halfway.
    ///
    /// The continuations are counted for EVERY new key, not only for one that adds an interval: unlike the
    /// preallocated modules, every new key reserves index bytes — its L2 frame — so a key that fits a page
    /// already there can still take the room a continuation reserved by an earlier interval needs.
    fn check_index_room(&self, new_page: bool) -> Result<(), ArchiveError> {
        let frames = (self.l0.len() + usize::from(new_page)).div_ceil(self.capacity_l0);
        let continuations = frames.saturating_sub(self.l0_chain.frames());

        let mut room = L2_FRAME_LEN + continuations * split_l0_frame_len::<TKey>(self.capacity_l0);

        if new_page {
            room += self.l1_page_len();
        }

        self.index.check_reservable(room).map(|_| ())
    }

    fn interval_of(&self, key: &TKey) -> usize {
        self.l0
            .partition_point(|entry| entry.from <= *key)
            .saturating_sub(1)
    }

    /// Inserts `entry` into the page of `interval`, halves the page if it overflows, and narrows the interval
    /// to what stays. Returns the upper half, empty when the page did not overflow.
    fn insert_into_page(&mut self, interval: usize, entry: (TKey, u32)) -> Vec<(TKey, u32)> {
        let capacity = self.capacity_l1;
        let page = self
            .index_pages
            .modify_l1(self.l0[interval].l1_offset)
            .expect("a placed key's pages are cached");

        let position = page
            .binary_search_by(|(stored, _)| stored.cmp(&entry.0))
            .unwrap_err();
        page.insert(position, entry);

        let upper_half = if page.len() > capacity {
            page.split_off(page.len() / 2)
        } else {
            Vec::new()
        };

        let (from, to) = (page[0].0.clone(), page[page.len() - 1].0.clone());
        self.l0[interval].from = from;
        self.l0[interval].to = to;

        upper_half
    }

    /// A new L1 page holding `entries` at the reserved `offset`, with its interval at L0 position `at`.
    fn put_new_page(&mut self, at: usize, offset: u32, entries: Vec<(TKey, u32)>) {
        self.l0.insert(
            at,
            SplitL0Interval {
                from: entries[0].0.clone(),
                to: entries[entries.len() - 1].0.clone(),
                l1_offset: offset,
            },
        );

        self.index_pages.insert_l1(offset, entries, true);
    }

    /// Caches the page of `interval`, evicting nothing. A page that is not cached is appended already — a
    /// page past the frontier is always cached — so it is read back through a reader of the index.
    async fn load_l1_page(&mut self, interval: usize) -> Result<(), ArchiveError> {
        let offset = self.l0[interval].l1_offset;

        if self.index_pages.touch_l1(offset) {
            return Ok(());
        }

        let len = self.l1_page_len();
        let bytes = self.index.read_back(&self.factory, offset, len).await?;
        let frame = SplitFrame::from_bytes(bytes, offset as u64, SplitFrameKind::L1)?;
        let entries = decode_records_l1_chain(&[frame], &self.l0[interval])?;

        self.index_pages.insert_l1(offset, entries, false);
        Ok(())
    }

    fn cached_l1_page(&self, offset: u32) -> &[(TKey, u32)] {
        self.index_pages
            .l1(offset)
            .expect("an L1 page just loaded is cached")
    }
}
