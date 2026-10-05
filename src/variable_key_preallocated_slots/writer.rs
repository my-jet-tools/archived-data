use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, CheckedItemBytes, CheckedSlotIndex,
    CheckedVariableKeyBytes, DEFAULT_L0_FRAME_BYTES, DEFAULT_L1_PAGE_BYTES,
    DEFAULT_MAX_DATA_FILE_BYTES, DataFileLayout, DataRef, InFlightGuard, IoReaderFactory,
    IoWriterFactory, PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyVariableSize,
    SPLIT_L0_HEADER_LEN, SPLIT_L1_HEADER_LEN, SplitDataWriters, SplitFrame, SplitFrameKind,
    SplitIndexWriter, SplitKeyPlacement, SplitL0Interval, SplitWorkingL0Chain,
    SplitWriterDataPages, SplitWriterL1Pages, check_variable_split_sizes,
    decode_variable_split_l1_chain, encode_split_l1_frame, encode_variable_split_l0_bodies,
    finish_split_writers, flush_split_working_archive, push_variable_split_l1_entry,
    refuse_split_oversized_key, variable_split_l0_entry_len, variable_split_l0_frames_bound,
    variable_split_l1_entry_len,
};

struct WriterL1Entry<TKey> {
    key: TKey,
    bytes: Vec<u8>,
    data: DataRef,
}

/// An interval, and the raw bytes of its bounds so L0 is encoded without asking the keys again.
struct WriterSplitL0Entry<TKey> {
    interval: SplitL0Interval<TKey>,
    from_bytes: Vec<u8>,
    to_bytes: Vec<u8>,
}

impl<TKey: Clone> WriterSplitL0Entry<TKey> {
    /// The bytes this interval takes in an L0 frame.
    fn len(&self) -> usize {
        variable_split_l0_entry_len(self.from_bytes.len(), self.to_bytes.len())
    }

    fn spanning(page: &[WriterL1Entry<TKey>], l1_offset: u32) -> Self {
        let first = &page[0];
        let last = &page[page.len() - 1];

        Self {
            interval: SplitL0Interval {
                from: first.key.clone(),
                to: last.key.clone(),
                l1_offset,
            },
            from_bytes: first.bytes.clone(),
            to_bytes: last.bytes.clone(),
        }
    }
}

/// Builds a WORKING split archive — an index and data files `0 ..= 255` — of variable-size keys, written
/// in any key order, through the writers (and, to read its own evicted L1 pages back, the readers) of one
/// factory.
///
/// The index holds L0 frame 0 at offset 0, appended when the writer is created with a hash that does not
/// verify, then L1 pages of `l1_page_bytes` each, and at flush the L0 continuations of `l0_frame_bytes` it
/// needs. Data pages go to the current data file while they fit whole in `max_data_file_bytes`, then to the
/// next one. A new key is inserted, sorted, into the L1 page of its interval and gets a data page
/// preallocated with `TItem::default()` in every slot; a known key has its item written into its slot in
/// place, in whichever data file the page lives.
///
/// A page whose entries no longer fit is not always split. A key past the last key of the LAST page, when
/// it does not fit there, starts a new page of its own, so keys written in ascending order fill every page.
/// Any other overflow splits the page by bytes — the left page keeps entries in key order while the next
/// one still fits — and the rest moves in front of the NEXT page's keys when it fits there whole; only when
/// it does not is a new page allocated.
///
/// A key whose L1 entry (prefix + key + 5) is more than half of what a page holds
/// (`2 × entry > l1_page_bytes − 12`) is refused as a `ContractViolation` before anything is written: that
/// bound is what makes every split produce exactly two pages that fit.
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
/// A refusal changes nothing, and the writer stays usable after it: a slot, an item or a key out of
/// contract, a key too large for the half-page rule, a new key that needs data file 256, or that needs
/// index bytes — a new L1 page, L0 continuations — the index has no room left for. Every one of them is
/// decided before the key changes anything. Any other `Err`, and a `write` or `flush` future dropped before
/// it returned — neither is cancel-safe — leaves every later `write`, `flush` and `finish` refused as a
/// `ContractViolation`; `finish` still finishes every file.
///
/// A key written before is never lost to a write that fails or is dropped: where a new key goes, and where
/// the part split off a page it overflows goes, is decided — with the pages involved read into the cache —
/// before the page is cut, and the cut is made with no await in it.
pub struct VariableKeyPreallocatedSlotsArchiveWriter<
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
    l0_frame_bytes: usize,
    l1_page_bytes: usize,
    l0: Vec<WriterSplitL0Entry<TPrimaryKey>>,
    /// The bytes every interval of `l0` takes in an L0 frame, kept with every interval added or changed.
    l0_entry_bytes: usize,
    l0_chain: SplitWorkingL0Chain,
    keys: usize,
    longest_key: usize,
    l1_pages: SplitWriterL1Pages<WriterL1Entry<TPrimaryKey>>,
    data_pages: SplitWriterDataPages<TSlotKey, TItem>,
    in_flight: InFlightGuard,
}

impl<TPrimaryKey, TSlotKey, TItem, TFactory, TWriter, TReader>
    VariableKeyPreallocatedSlotsArchiveWriter<
        TPrimaryKey,
        TSlotKey,
        TItem,
        TFactory,
        TWriter,
        TReader,
    >
where
    TPrimaryKey: PrimaryKeyVariableSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TFactory: IoWriterFactory<TWriter> + IoReaderFactory<TReader>,
    TWriter: ArchiveFileWriter,
    TReader: ArchiveFileReader,
{
    /// [`DEFAULT_L0_FRAME_BYTES`] per L0 frame, [`DEFAULT_L1_PAGE_BYTES`] per L1 page.
    pub async fn new(factory: TFactory) -> Result<Self, ArchiveError> {
        Self::new_with_capacity(factory, DEFAULT_L0_FRAME_BYTES, DEFAULT_L1_PAGE_BYTES).await
    }

    /// Both sizes in BYTES, headers included. `l0_frame_bytes` is per L0 FRAME, not a limit on L0.
    /// Creates the index and appends frame 0; data file 0 is created with the first key.
    ///
    /// Panics when an L1 page cannot hold two entries of a one-byte key (`12 + 2 × 7`), when an L0 frame
    /// cannot hold one interval of the largest key such a page accepts (`20 + 2 × min((l1_page_bytes −
    /// 12) / 2 − 5, 2 + 32767) + 4`), or when a size does not fit `u32`.
    pub async fn new_with_capacity(
        factory: TFactory,
        l0_frame_bytes: usize,
        l1_page_bytes: usize,
    ) -> Result<Self, ArchiveError> {
        check_variable_split_sizes::<TSlotKey, TItem>(l0_frame_bytes, l1_page_bytes);

        let l0_chain = SplitWorkingL0Chain::new(l0_frame_bytes);
        let index = SplitIndexWriter::create(&factory, &l0_chain.unflushed_frame()?).await?;

        let data_pages = SplitWriterDataPages::new();
        let layout = DataFileLayout::new(data_pages.page_len(), DEFAULT_MAX_DATA_FILE_BYTES);

        Ok(Self {
            factory,
            index,
            data_files: SplitDataWriters::new(layout),
            l0_frame_bytes,
            l1_page_bytes,
            l0: Vec::new(),
            l0_entry_bytes: 0,
            l0_chain,
            keys: 0,
            longest_key: 0,
            l1_pages: SplitWriterL1Pages::new(l1_page_bytes),
            data_pages,
            in_flight: InFlightGuard::new(),
        })
    }

    /// At least 1. A smaller cache only costs IO. A write placing a new key holds up to three pages more
    /// until it returns: the key's page, the next one, and a new one.
    pub fn set_l1_cache_size(&mut self, pages: usize) {
        self.l1_pages.set_cache_size(pages);
    }

    /// 0 appends every new data page at once and writes every item straight to its data file.
    pub fn set_data_cache_size(&mut self, pages: usize) {
        self.data_pages.set_cache_size(pages);
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

    pub fn keys_count(&self) -> usize {
        self.keys
    }

    pub fn l0_intervals(&self) -> usize {
        self.l0.len()
    }

    /// The L0 frames the index has room for so far: frame 0 plus every continuation a flush reserved. A
    /// flush may link fewer, when the bounds of the intervals got shorter.
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

        let mut key_bytes = Vec::new();
        key.copy_variable_key_checked_to(&mut key_bytes)?;
        refuse_split_oversized_key(key_bytes.len(), self.l1_page_bytes)?;

        self.in_flight.begin()?;

        let at = match self.data_ref_of(&key).await? {
            Some(at) => at,
            None => match self.place_new_key(&key, key_bytes.len()).await? {
                Ok(placement) => self.insert_key(key, key_bytes, placement).await?,
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
        let l0_bodies = encode_variable_split_l0_bodies(
            self.l0.iter().map(|entry| {
                (
                    &entry.from_bytes[..],
                    &entry.to_bytes[..],
                    entry.interval.l1_offset,
                )
            }),
            self.l0_body_bytes(),
        );

        flush_split_working_archive(
            &mut self.index,
            &mut self.l0_chain,
            &mut self.l1_pages,
            &l1_encoder(self.l1_page_bytes),
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

    /// The bytes an L1 page has for entries.
    fn room(&self) -> usize {
        self.l1_page_bytes - SPLIT_L1_HEADER_LEN
    }

    /// The bytes an L0 frame has for entries.
    fn l0_body_bytes(&self) -> usize {
        self.l0_frame_bytes - SPLIT_L0_HEADER_LEN
    }

    fn interval_of(&self, key: &TPrimaryKey) -> usize {
        self.l0
            .partition_point(|entry| entry.interval.from <= *key)
            .saturating_sub(1)
    }

    fn l1_offset(&self, interval: usize) -> u64 {
        self.l0[interval].interval.l1_offset as u64
    }

    async fn data_ref_of(&mut self, key: &TPrimaryKey) -> Result<Option<DataRef>, ArchiveError> {
        if self.l0.is_empty() {
            return Ok(None);
        }

        let interval = self.interval_of(key);
        self.load_l1_page(interval).await?;

        let page = self.cached_l1_page(self.l1_offset(interval));

        Ok(page
            .binary_search_by(|stored| stored.key.cmp(key))
            .ok()
            .map(|at| page[at].data))
    }

    /// Where a key that is not in the archive goes, decided before anything changes. The inner `Err` is a
    /// refusal that changed nothing, after which the writer stays usable: a key needing data file 256, an
    /// index with no room for the bytes the key needs. The outer one is a failed read.
    ///
    /// The pages it reads only join the cache — the key's page and, when that overflows, the next one —
    /// and nothing is evicted until the key is placed, so both are still cached when the page is cut.
    async fn place_new_key(
        &mut self,
        key: &TPrimaryKey,
        key_len: usize,
    ) -> Result<Result<SplitKeyPlacement, ArchiveError>, ArchiveError> {
        if let Err(refused) = self.data_files.layout().ref_of(self.keys) {
            return Ok(Err(refused));
        }

        if self.l0.is_empty() {
            return Ok(self
                .check_index_room(key_len, true)
                .map(|()| SplitKeyPlacement::NewPage { at: 0 }));
        }

        let interval = self.interval_of(key);
        self.load_l1_page(interval).await?;

        let page = self.cached_l1_page(self.l1_offset(interval));
        let position = page
            .binary_search_by(|stored| stored.key.cmp(key))
            .unwrap_err();
        let entry_len = variable_split_l1_entry_len(key_len);

        let Some(upper_part) = split_off_len(page, position, entry_len, self.room()) else {
            return Ok(self
                .check_index_room(key_len, false)
                .map(|()| SplitKeyPlacement::Insert { interval }));
        };

        let next = interval + 1;

        if next == self.l0.len() && *key > self.l0[interval].interval.to {
            return Ok(self
                .check_index_room(key_len, true)
                .map(|()| SplitKeyPlacement::NewPage { at: next }));
        }

        if next < self.l0.len() {
            self.load_l1_page(next).await?;

            if entries_len(self.cached_l1_page(self.l1_offset(next))) + upper_part <= self.room() {
                return Ok(self
                    .check_index_room(key_len, false)
                    .map(|()| SplitKeyPlacement::SplitIntoNext { interval }));
            }
        }

        Ok(self
            .check_index_room(key_len, true)
            .map(|()| SplitKeyPlacement::SplitIntoNewPage { interval }))
    }

    /// The index bytes a key of `key_len` bytes may need must fit the index: its new L1 page, and the L0
    /// continuations a flush will need — so neither placing the key nor a later flush can run into the
    /// index limit halfway.
    ///
    /// L0 entries vary in size, so the continuations are bounded rather than counted, from what L0 will be
    /// once the key is placed (see [`variable_split_l0_frames_bound`]): no L0 entry is longer than both
    /// bounds of the longest key written, and L0's bytes grow by at most two such entries — a split
    /// changes one interval and adds or changes another. A longer key can need a continuation although it
    /// needs no page.
    ///
    /// Taken by count alone, one long key would count every interval as long as its own: with a key near
    /// the half-page limit, the continuations reserved would match the L1 pages byte for byte, and the
    /// index would refuse new keys at half its size. The bound by bytes counts the intervals at what they
    /// are, so such a key costs a frame or so.
    fn check_index_room(&self, key_len: usize, new_page: bool) -> Result<(), ArchiveError> {
        let longest = self.longest_key.max(key_len);
        let longest_entry = variable_split_l0_entry_len(longest, longest);

        let frames = variable_split_l0_frames_bound(
            self.l0.len() + usize::from(new_page),
            self.l0_entry_bytes + 2 * longest_entry,
            longest_entry,
            self.l0_body_bytes(),
        );
        let continuations = frames.saturating_sub(self.l0_chain.frames());
        let page = if new_page { self.l1_page_bytes } else { 0 };
        let needed = page + continuations * self.l0_frame_bytes;

        if needed == 0 {
            return Ok(());
        }

        self.index.check_reservable(needed).map(|_| ())
    }

    /// Places a key where [`Self::place_new_key`] decided. Its data page is preallocated first; after that
    /// the key goes into L1 with no await between the first change and the last, so a failed or dropped
    /// write never leaves an L1 page cut with its upper part nowhere. An L1 page is never cached while its
    /// entries do not fit it, because an eviction may encode any cached page.
    async fn insert_key(
        &mut self,
        key: TPrimaryKey,
        key_bytes: Vec<u8>,
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

        let key_len = key_bytes.len();
        let entry = WriterL1Entry {
            key,
            bytes: key_bytes,
            data: at,
        };

        match placement {
            SplitKeyPlacement::NewPage { at: position } => {
                let offset = self.index.reserve(self.l1_page_bytes)?;
                self.put_new_page(position, offset, vec![entry]);
            }
            SplitKeyPlacement::Insert { interval } => {
                let upper_part = self.insert_into_page(interval, entry);
                debug_assert!(upper_part.is_empty());
            }
            SplitKeyPlacement::SplitIntoNext { interval } => {
                let upper_part = self.insert_into_page(interval, entry);
                let offset = self.l0[interval + 1].interval.l1_offset;

                let page = self
                    .l1_pages
                    .modify(offset as u64)
                    .expect("a placed key's pages are cached");
                page.splice(0..0, upper_part);
                let next = WriterSplitL0Entry::spanning(page, offset);
                self.replace_l0_entry(interval + 1, next);
            }
            SplitKeyPlacement::SplitIntoNewPage { interval } => {
                let offset = self.index.reserve(self.l1_page_bytes)?;
                let upper_part = self.insert_into_page(interval, entry);
                self.put_new_page(interval + 1, offset, upper_part);
            }
        }

        self.longest_key = self.longest_key.max(key_len);
        self.keys += 1;
        Ok(at)
    }

    /// Inserts `entry` into the page of `interval`, splits the page by bytes if its entries no longer fit,
    /// and narrows the interval to what stays. Returns the part split off, empty when nothing was.
    fn insert_into_page(
        &mut self,
        interval: usize,
        entry: WriterL1Entry<TPrimaryKey>,
    ) -> Vec<WriterL1Entry<TPrimaryKey>> {
        let room = self.room();
        let offset = self.l0[interval].interval.l1_offset;

        let page = self
            .l1_pages
            .modify(offset as u64)
            .expect("a placed key's pages are cached");

        let position = page
            .binary_search_by(|stored| stored.key.cmp(&entry.key))
            .unwrap_err();
        page.insert(position, entry);

        let upper_part = split_off_overflow(page, room);
        let spanning = WriterSplitL0Entry::spanning(page, offset);
        self.replace_l0_entry(interval, spanning);

        upper_part
    }

    /// A new L1 page holding `entries` at the reserved `offset`, with its interval at L0 position `at`.
    fn put_new_page(&mut self, at: usize, offset: u32, entries: Vec<WriterL1Entry<TPrimaryKey>>) {
        let spanning = WriterSplitL0Entry::spanning(&entries, offset);

        self.l0_entry_bytes += spanning.len();
        self.l0.insert(at, spanning);
        self.l1_pages.insert(offset as u64, entries, true);
    }

    fn replace_l0_entry(&mut self, at: usize, entry: WriterSplitL0Entry<TPrimaryKey>) {
        self.l0_entry_bytes = self.l0_entry_bytes - self.l0[at].len() + entry.len();
        self.l0[at] = entry;
    }

    /// Caches the page of `interval`, evicting nothing. A page that is not cached is appended already — a
    /// page past the frontier is always cached — so it is read back through a reader of the index.
    async fn load_l1_page(&mut self, interval: usize) -> Result<(), ArchiveError> {
        let offset = self.l0[interval].interval.l1_offset;

        if self.l1_pages.touch(offset as u64) {
            return Ok(());
        }

        let bytes = self
            .index
            .read_back(&self.factory, offset, self.l1_page_bytes)
            .await?;
        let frames = [SplitFrame::from_bytes(
            bytes,
            offset as u64,
            SplitFrameKind::L1,
        )?];

        let entries = decode_variable_split_l1_chain(&frames, &self.l0[interval].interval)?
            .into_iter()
            .map(|entry| WriterL1Entry {
                bytes: frames[entry.frame].body[entry.key_at..entry.key_at + entry.key_len]
                    .to_vec(),
                key: entry.key,
                data: entry.data,
            })
            .collect();

        self.l1_pages.insert(offset as u64, entries, false);

        Ok(())
    }

    /// Evicts down to the cache size, once a write no longer needs the pages it read in.
    async fn evict_l1_pages(&mut self) -> Result<(), ArchiveError> {
        let keep = self.l1_pages.cache_size();

        self.l1_pages
            .evict(&mut self.index, keep, &l1_encoder(self.l1_page_bytes))
            .await
    }

    fn cached_l1_page(&self, offset: u64) -> &[WriterL1Entry<TPrimaryKey>] {
        self.l1_pages
            .get(offset)
            .expect("an L1 page just loaded is cached")
    }
}

fn l1_encoder<TKey>(
    size: usize,
) -> impl Fn(&[WriterL1Entry<TKey>]) -> Result<Vec<u8>, ArchiveError> {
    move |entries| encode_l1_page(entries, size)
}

fn entries_len<TKey>(entries: &[WriterL1Entry<TKey>]) -> usize {
    entries
        .iter()
        .map(|entry| variable_split_l1_entry_len(entry.bytes.len()))
        .sum()
}

/// ONE frame (`next` 0) of `size` bytes, zero-padded past the entries: the first zero is the end marker.
fn encode_l1_page<TKey>(
    entries: &[WriterL1Entry<TKey>],
    size: usize,
) -> Result<Vec<u8>, ArchiveError> {
    let mut body = Vec::with_capacity(entries_len(entries));

    for entry in entries {
        push_variable_split_l1_entry(&mut body, &entry.bytes, entry.data);
    }

    encode_split_l1_frame(size, 0, entries.len(), &body)
}

/// The entries past the first one that no longer fits `room` bytes, empty if every one fits. With every
/// entry at most half of `room`, the left part keeps more than half and the right part fits a page of its
/// own.
fn split_off_overflow<TKey>(
    entries: &mut Vec<WriterL1Entry<TKey>>,
    room: usize,
) -> Vec<WriterL1Entry<TKey>> {
    let mut filled = 0;

    for (index, entry) in entries.iter().enumerate() {
        filled += variable_split_l1_entry_len(entry.bytes.len());

        if filled > room {
            return entries.split_off(index);
        }
    }

    Vec::new()
}

/// The bytes of the entries [`split_off_overflow`] would split off `page` once an entry of `entry_len`
/// bytes is inserted at `position`, reading the page only; `None` when nothing would be.
fn split_off_len<TKey>(
    page: &[WriterL1Entry<TKey>],
    position: usize,
    entry_len: usize,
    room: usize,
) -> Option<usize> {
    let lens = page[..position]
        .iter()
        .map(|stored| variable_split_l1_entry_len(stored.bytes.len()))
        .chain(std::iter::once(entry_len))
        .chain(
            page[position..]
                .iter()
                .map(|stored| variable_split_l1_entry_len(stored.bytes.len())),
        );

    let total = entries_len(page) + entry_len;
    let mut filled = 0;

    for len in lens {
        filled += len;

        if filled > room {
            return Some(total - (filled - len));
        }
    }

    None
}
