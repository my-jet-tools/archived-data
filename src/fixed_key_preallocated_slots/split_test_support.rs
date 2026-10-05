//! What the split tests of this module share: the rows they write, the archives they build over
//! `InMemoryIo`, a parser that checks every invariant of an index straight off its bytes, and two archives
//! written with literal sizes and none of the crate's code — the reference compact archive, and a
//! hand-built one whose frames are chained. What does not depend on the key comes from the crate root.

use std::collections::BTreeMap;

use super::test_types::{Hour, Login, Record, Rng};
use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
use crate::{
    DataRef, FixedKeyPreallocatedSlotsArchiveReader, FixedKeyPreallocatedSlotsArchiveWriter,
    data_frame, l0_frame, l0_hash, l1_frame,
};

pub use crate::split_test_io::{
    Gate, NOT_USABLE, assert_still_waiting, data_files, data_read_ats, data_readers_opened,
    drop_when_parked, file_bytes, poll_until_parked, refused, wait_until,
};
pub use crate::test_items::{DATA_PAGE, DEFAULT_PAGES_PER_FILE, seeded};
pub(crate) use crate::{frame_offsets, le_u32_at, le_u64_at, patch_u16, patch_u64};

pub type Writer = FixedKeyPreallocatedSlotsArchiveWriter<
    Login,
    Hour,
    Record,
    InMemoryIo,
    InMemoryWriter,
    InMemoryReader,
>;
pub type Reader =
    FixedKeyPreallocatedSlotsArchiveReader<Login, Hour, Record, InMemoryIo, InMemoryReader>;

pub type Day = BTreeMap<u64, Vec<Option<Record>>>;
pub type Row = (u64, usize, Record);

/// `from u64 · to u64 · l1_offset u32`.
pub const L0_ENTRY: usize = 20;
/// `key u64 · file_no u8 · offset u32`.
pub const L1_ENTRY: usize = 13;

pub fn l0_frame_len(intervals: usize) -> usize {
    20 + intervals * L0_ENTRY
}

pub fn l1_page_len(capacity: usize) -> usize {
    12 + capacity * L1_ENTRY
}

pub fn logins(count: u64, rng: &mut Rng) -> Vec<u64> {
    let mut logins: Vec<u64> = (0..count).map(|i| i * 3 + 1_000 + rng.next() % 2).collect();
    logins.sort_unstable();
    logins.dedup();
    logins
}

/// `logins` × up to three distinct hours each, as `(login, hour, record)` fully shuffled. Each slot is
/// written once, so every order of the rows describes the same day.
pub fn rows(logins: &[u64], rng: &mut Rng) -> (Vec<Row>, Day) {
    let mut rows = Vec::new();
    let mut day = Day::new();

    for login in logins {
        let hours = day.entry(*login).or_insert_with(|| vec![None; 24]);

        for _ in 0..3 {
            let hour = (rng.next() % 24) as usize;

            if hours[hour].is_some() {
                continue;
            }

            let record = Record::with(login * 100 + hour as u64 + rng.next() % 7);
            hours[hour] = Some(record.clone());
            rows.push((*login, hour, record));
        }
    }

    rng.shuffle(&mut rows);
    (rows, day)
}

/// The day a prefix of the rows describes.
pub fn day_of(rows: &[Row]) -> Day {
    let mut day = Day::new();

    for (login, hour, record) in rows {
        day.entry(*login).or_insert_with(|| vec![None; 24])[*hour] = Some(record.clone());
    }

    day
}

pub async fn write_all(writer: &mut Writer, rows: &[Row]) {
    for (login, hour, record) in rows {
        writer
            .write(Login(*login), Hour(*hour), record)
            .await
            .unwrap();
    }
}

/// A finished working archive of `rows`. `cache` is both caches; `pages_per_file` sets
/// `max_data_file_bytes` to that many data pages.
pub async fn build(
    rows: &[Row],
    capacity_l0: usize,
    capacity_l1: usize,
    cache: usize,
    pages_per_file: Option<usize>,
) -> InMemoryIo {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), capacity_l0, capacity_l1)
        .await
        .unwrap();

    writer.set_l1_cache_size(cache);
    writer.set_data_cache_size(cache);

    if let Some(pages) = pages_per_file {
        writer.set_max_data_file_bytes((pages * DATA_PAGE) as u64);
    }

    write_all(&mut writer, rows).await;
    writer.finish().await.unwrap();
    io
}

/// The compact archive of `reader`, under the reader's `max_data_file_bytes`.
pub async fn compact(reader: &Reader) -> InMemoryIo {
    let out = InMemoryIo::default();
    let plan = reader.compaction_plan().await.unwrap();
    reader.write_compacted_to(&plan, &out).await.unwrap();
    out
}

pub async fn assert_reads_back(reader: &Reader, day: &Day) {
    for (login, hours) in day {
        let items = reader
            .get(&Login(*login), Hour(0), Hour(23))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("login {login} is absent"));

        let expected: Vec<Record> = hours
            .iter()
            .map(|hour| hour.clone().unwrap_or_default())
            .collect();
        assert_eq!(items, expected, "login {login}");
    }
}

/// An index as its bytes hold it.
pub struct ParsedIndex {
    pub intervals: Vec<(u64, u64, u32)>,
    pub pages: Vec<Vec<(u64, DataRef)>>,
}

impl ParsedIndex {
    pub fn counts(&self) -> Vec<usize> {
        self.pages.iter().map(Vec::len).collect()
    }

    pub fn keys(&self) -> Vec<Vec<u64>> {
        self.pages
            .iter()
            .map(|page| page.iter().map(|(key, _)| *key).collect())
            .collect()
    }

    pub fn refs(&self) -> Vec<DataRef> {
        self.pages
            .iter()
            .flat_map(|page| page.iter().map(|(_, data)| *data))
            .collect()
    }
}

/// Parses an index a writer or a compaction produced, asserting every invariant on the way: every L1 page
/// is one frame of exactly `capacity_l1` entries holding 1 to `capacity_l1` strictly ascending keys, it
/// spans its interval exactly, and the intervals are ordered and disjoint across the whole chain.
pub fn parse_index(index: &[u8], capacity_l1: usize) -> ParsedIndex {
    let mut intervals: Vec<(u64, u64, u32)> = Vec::new();

    for frame in frame_offsets(index) {
        let size = le_u32_at(index, frame) as usize;
        let count = le_u32_at(index, frame + 8) as usize;
        assert!(
            20 + count * L0_ENTRY <= size,
            "the L0 frame at {frame} overflows"
        );
        assert_eq!(
            le_u64_at(index, frame + 12),
            l0_hash(size as u32, le_u32_at(index, frame + 4), count as u32),
            "the hash of the L0 frame at {frame}"
        );

        for entry in 0..count {
            let at = frame + 20 + entry * L0_ENTRY;
            intervals.push((
                le_u64_at(index, at),
                le_u64_at(index, at + 8),
                le_u32_at(index, at + 16),
            ));
        }
    }

    let mut pages = Vec::new();

    for (n, (from, to, offset)) in intervals.iter().enumerate() {
        assert!(from <= to, "interval {n} is upside down");
        assert!(
            n == 0 || intervals[n - 1].1 < *from,
            "interval {n} is not after the previous one"
        );

        let at = *offset as usize;
        assert_eq!(
            le_u32_at(index, at) as usize,
            l1_page_len(capacity_l1),
            "the size of the L1 page at {at}"
        );
        assert_eq!(le_u32_at(index, at + 4), 0, "the L1 page at {at} chains");

        let count = le_u32_at(index, at + 8) as usize;
        assert!(
            (1..=capacity_l1).contains(&count),
            "the L1 page at {at} holds {count} keys"
        );

        let entries: Vec<(u64, DataRef)> = (0..count)
            .map(|entry| {
                let at = at + 12 + entry * L1_ENTRY;
                let data = DataRef {
                    file_no: index[at + 8],
                    offset: le_u32_at(index, at + 9),
                };
                (le_u64_at(index, at), data)
            })
            .collect();

        assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the keys of the L1 page at {at} do not ascend"
        );
        assert_eq!(
            (entries[0].0, entries[count - 1].0),
            (*from, *to),
            "the L1 page at {at} does not span its interval"
        );

        pages.push(entries);
    }

    let mut offsets: Vec<u32> = intervals.iter().map(|(_, _, offset)| *offset).collect();
    offsets.sort_unstable();
    offsets.dedup();
    assert_eq!(offsets.len(), intervals.len(), "two intervals share a page");

    ParsedIndex { intervals, pages }
}

/// The compact split archive of `day`, from the written-down layout with its literal sizes: the index
/// (L0 `20 + 20 × p`, L1 pages of `12 + 13 × 1024`) and the data files of `pages_per_file` pages of
/// `10 + 24 × 128` each.
pub fn reference_split_day(day: &Day, pages_per_file: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    const L1_CAPACITY: usize = 1024;
    const L1_PAGE: usize = 12 + 13 * L1_CAPACITY;

    let logins: Vec<u64> = day.keys().copied().collect();
    let pages: Vec<&[u64]> = logins.chunks(L1_CAPACITY).collect();
    let l0_len = 20 + 20 * pages.len();

    let mut index = Vec::new();
    index.extend_from_slice(&(l0_len as u32).to_le_bytes());
    index.extend_from_slice(&0u32.to_le_bytes());
    index.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    index.extend_from_slice(&l0_hash(l0_len as u32, 0, pages.len() as u32).to_le_bytes());

    for (page, chunk) in pages.iter().enumerate() {
        index.extend_from_slice(&chunk[0].to_le_bytes());
        index.extend_from_slice(&chunk[chunk.len() - 1].to_le_bytes());
        index.extend_from_slice(&((l0_len + page * L1_PAGE) as u32).to_le_bytes());
    }

    for (page, chunk) in pages.iter().enumerate() {
        let start = index.len();
        index.extend_from_slice(&(L1_PAGE as u32).to_le_bytes());
        index.extend_from_slice(&0u32.to_le_bytes());
        index.extend_from_slice(&(chunk.len() as u32).to_le_bytes());

        for (entry, login) in chunk.iter().enumerate() {
            let rank = page * L1_CAPACITY + entry;
            index.extend_from_slice(&login.to_le_bytes());
            index.push((rank / pages_per_file) as u8);
            index.extend_from_slice(&((rank % pages_per_file * DATA_PAGE) as u32).to_le_bytes());
        }

        index.resize(start + L1_PAGE, 0);
    }

    let mut data: Vec<Vec<u8>> = Vec::new();

    for (rank, hours) in day.values().enumerate() {
        if rank % pages_per_file == 0 {
            data.push(Vec::new());
        }

        let file = data.last_mut().unwrap();
        file.extend_from_slice(&(DATA_PAGE as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&128u16.to_le_bytes());

        for hour in hours {
            file.extend_from_slice(&hour.clone().unwrap_or_default().0);
        }
    }

    (index, data)
}

/// Where each frame of [`hand_built`] lies. The index: L0 frame 0 → an L1 page chained over two frames,
/// the second at the very end → an L0 continuation after that first L1 frame → a one-frame L1 page.
pub const HB_L0_0: usize = 0;
pub const HB_L1_A: usize = 40;
pub const HB_L0_1: usize = 78;
pub const HB_L1_B: usize = 118;
pub const HB_L1_A2: usize = 143;
pub const HB_INDEX_LEN: usize = 168;

/// Data file 0: key 1 in one frame, key 2 in three frames with filler between them, keys 3 and 10.
pub const HB_K1: usize = 0;
pub const HB_K2_F1: usize = 3082;
pub const HB_K2_F2: usize = 4100;
pub const HB_K2_F3: usize = 4620;
pub const HB_K3: usize = 6202;
pub const HB_K10: usize = 9284;
pub const HB_DATA_LEN: usize = 12366;

/// An archive no writer produces: chained frames on L0, L1 and data. Keys 1, 2, 3 in interval 0 and 10 in
/// interval 1; a key's slots `h` with `h % 5 == key % 5` are written.
pub struct HandBuilt {
    pub index: Vec<u8>,
    pub data: Vec<u8>,
    pub day: Day,
}

impl HandBuilt {
    pub fn io(&self) -> InMemoryIo {
        let io = InMemoryIo::default();
        io.put(FileRole::Index, self.index.clone());
        io.put(FileRole::Data(0), self.data.clone());
        io
    }
}

pub fn hand_built() -> HandBuilt {
    let mut day = Day::new();

    for login in [1u64, 2, 3, 10] {
        let hours = (0..24u64)
            .map(|hour| (hour % 5 == login % 5).then(|| Record::with(login * 100 + hour)))
            .collect();
        day.insert(login, hours);
    }

    let body = |login: u64| -> Vec<u8> {
        day[&login]
            .iter()
            .flat_map(|hour| hour.clone().unwrap_or_default().0)
            .collect()
    };

    let interval = |from: u64, to: u64, l1_offset: usize| {
        [
            &from.to_le_bytes()[..],
            &to.to_le_bytes(),
            &(l1_offset as u32).to_le_bytes(),
        ]
        .concat()
    };

    let entry = |key: u64, offset: usize| {
        [&key.to_le_bytes()[..], &[0], &(offset as u32).to_le_bytes()].concat()
    };

    let mut index = l0_frame(40, HB_L0_1 as u32, 1, &interval(1, 3, HB_L1_A));
    index.extend_from_slice(&l1_frame(
        38,
        HB_L1_A2 as u32,
        2,
        &[entry(1, HB_K1), entry(2, HB_K2_F1)].concat(),
    ));
    index.extend_from_slice(&l0_frame(40, 0, 1, &interval(10, 10, HB_L1_B)));
    index.extend_from_slice(&l1_frame(25, 0, 1, &entry(10, HB_K10)));
    index.extend_from_slice(&l1_frame(25, 0, 1, &entry(3, HB_K3)));
    assert_eq!(index.len(), HB_INDEX_LEN);

    let two = body(2);
    let mut data = data_frame(3082, 0, 128, &body(1));
    data.extend_from_slice(&data_frame(1010, HB_K2_F2 as u32, 128, &two[..1000]));
    data.resize(HB_K2_F2, 0xCC);
    data.extend_from_slice(&data_frame(510, HB_K2_F3 as u32, 128, &two[1000..1500]));
    data.resize(HB_K2_F3, 0xCC);
    data.extend_from_slice(&data_frame(1582, 0, 128, &two[1500..]));
    data.extend_from_slice(&data_frame(3082, 0, 128, &body(3)));
    data.extend_from_slice(&data_frame(3082, 0, 128, &body(10)));
    assert_eq!(data.len(), HB_DATA_LEN);

    HandBuilt { index, data, day }
}
