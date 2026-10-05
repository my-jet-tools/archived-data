//! What the split tests of this module share: the rows they write, the archives they build over
//! `InMemoryIo`, a parser that checks every invariant of an index straight off its bytes, and two archives
//! written with literal sizes and none of the crate's code — the reference compact archive, and a
//! hand-built one whose frames are chained. What does not depend on the key comes from the crate root.

use std::collections::{BTreeMap, BTreeSet};

use super::test_types::{Hour, Name, Names, Record, Rng};
use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
use crate::{
    DataRef, VariableKeyPreallocatedSlotsArchiveReader, VariableKeyPreallocatedSlotsArchiveWriter,
    data_frame, l0_frame, l0_hash, l1_frame,
};

pub use crate::split_test_io::{
    Gate, NOT_USABLE, assert_still_waiting, data_files, data_read_ats, data_readers_opened,
    drop_when_parked, file_bytes, poll_until_parked, refused, wait_until,
};
pub use crate::test_items::{DATA_PAGE, DEFAULT_PAGES_PER_FILE, seeded};
pub(crate) use crate::{frame_offsets, le_u32_at, le_u64_at, patch_u16};

pub type Writer = VariableKeyPreallocatedSlotsArchiveWriter<
    Name,
    Hour,
    Record,
    InMemoryIo,
    InMemoryWriter,
    InMemoryReader,
>;
pub type Reader =
    VariableKeyPreallocatedSlotsArchiveReader<Name, Hour, Record, InMemoryIo, InMemoryReader>;

pub type Row = (Vec<u8>, usize, Record);

/// `count` distinct keys of 1..=`max_len` random bytes, zero bytes included.
pub fn random_keys(count: usize, max_len: u64, rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut keys = BTreeSet::new();

    while keys.len() < count {
        let len = 1 + rng.next() % max_len;
        keys.insert((0..len).map(|_| rng.next() as u8).collect::<Vec<u8>>());
    }

    keys.into_iter().collect()
}

/// `key-00042`: nine bytes, and the numbers sort as the bytes do.
pub fn key(n: u64) -> Vec<u8> {
    format!("key-{n:05}").into_bytes()
}

pub fn numbered_keys(count: u64) -> Vec<Vec<u8>> {
    (0..count).map(key).collect()
}

/// Every key × up to three distinct hours, fully shuffled. Each slot is written once, so every order of
/// the rows describes the same archive.
pub fn rows(keys: &[Vec<u8>], rng: &mut Rng) -> (Vec<Row>, Names) {
    let mut rows = Vec::new();
    let mut names = Names::new();

    for key in keys {
        let hours = names.entry(key.clone()).or_insert_with(|| vec![None; 24]);

        for _ in 0..3 {
            let hour = (rng.next() % 24) as usize;

            if hours[hour].is_some() {
                continue;
            }

            let record = Record::with(rng.next() % 1_000_000);
            hours[hour] = Some(record.clone());
            rows.push((key.clone(), hour, record));
        }
    }

    rng.shuffle(&mut rows);
    (rows, names)
}

/// The names a prefix of the rows describes.
pub fn names_of(rows: &[Row]) -> Names {
    let mut names = Names::new();

    for (key, hour, record) in rows {
        names.entry(key.clone()).or_insert_with(|| vec![None; 24])[*hour] = Some(record.clone());
    }

    names
}

/// One row per key, in slot 0, with a value that follows from the key.
pub fn hour_0_rows(keys: &[Vec<u8>]) -> Vec<Row> {
    keys.iter()
        .map(|key| {
            let value = key.iter().map(|byte| *byte as u64).sum();
            (key.clone(), 0, Record::with(value))
        })
        .collect()
}

pub async fn write_all(writer: &mut Writer, rows: &[Row]) {
    for (key, hour, record) in rows {
        writer
            .write(Name(key.clone()), Hour(*hour), record)
            .await
            .unwrap();
    }
}

/// A finished working archive of `rows`. `cache` is both caches; `pages_per_file` sets
/// `max_data_file_bytes` to that many data pages.
pub async fn build(
    rows: &[Row],
    l0_frame_bytes: usize,
    l1_page_bytes: usize,
    cache: usize,
    pages_per_file: Option<usize>,
) -> InMemoryIo {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), l0_frame_bytes, l1_page_bytes)
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

pub async fn assert_reads_back(reader: &Reader, names: &Names) {
    for (key, hours) in names {
        let items = reader
            .get(&Name(key.clone()), Hour(0), Hour(23))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("key {key:?} is absent"));

        let expected: Vec<Record> = hours
            .iter()
            .map(|hour| hour.clone().unwrap_or_default())
            .collect();
        assert!(items == expected, "key {key:?}");
    }
}

/// A length prefix and the key, written out by hand.
pub fn prefixed(key: &[u8]) -> Vec<u8> {
    let mut out = if key.len() < 128 {
        vec![key.len() as u8]
    } else {
        (key.len() as u16 | 0x8000).to_be_bytes().to_vec()
    };
    out.extend_from_slice(key);
    out
}

/// The key whose prefix starts at `at`, and where the bytes after it start.
fn key_at(bytes: &[u8], at: usize) -> (Vec<u8>, usize) {
    let first = bytes[at];
    assert_ne!(first, 0, "an empty key at {at}");

    let (len, start) = if first < 0x80 {
        (first as usize, at + 1)
    } else {
        (
            (((first & 0x7F) as usize) << 8) | bytes[at + 1] as usize,
            at + 2,
        )
    };

    (bytes[start..start + len].to_vec(), start + len)
}

/// An index as its bytes hold it.
pub struct ParsedIndex {
    pub intervals: Vec<(Vec<u8>, Vec<u8>, u32)>,
    pub pages: Vec<Vec<(Vec<u8>, DataRef)>>,
    pub sizes: Vec<usize>,
}

impl ParsedIndex {
    pub fn counts(&self) -> Vec<usize> {
        self.pages.iter().map(Vec::len).collect()
    }

    pub fn keys(&self) -> Vec<Vec<Vec<u8>>> {
        self.pages
            .iter()
            .map(|page| page.iter().map(|(key, _)| key.clone()).collect())
            .collect()
    }

    pub fn refs(&self) -> Vec<DataRef> {
        self.pages
            .iter()
            .flat_map(|page| page.iter().map(|(_, data)| *data))
            .collect()
    }

    /// The bytes of entries on each page.
    pub fn entry_bytes(&self) -> Vec<usize> {
        self.pages
            .iter()
            .map(|page| page.iter().map(|(key, _)| prefixed(key).len() + 5).sum())
            .collect()
    }
}

/// Parses an index a writer (`l1_page_bytes` given: every page that size, zero-padded) or a compaction
/// (`None`: every page exactly its entries) produced, asserting every invariant on the way: every L1 page
/// is one frame of 1 or more strictly ascending keys whose entries end where the frame or its padding
/// does, it spans its interval exactly, and the intervals are ordered and disjoint across the whole chain.
pub fn parse_index(index: &[u8], l1_page_bytes: Option<usize>) -> ParsedIndex {
    let mut intervals: Vec<(Vec<u8>, Vec<u8>, u32)> = Vec::new();

    for frame in frame_offsets(index) {
        let end = frame + le_u32_at(index, frame) as usize;
        let count = le_u32_at(index, frame + 8) as usize;
        assert_eq!(
            le_u64_at(index, frame + 12),
            l0_hash(
                (end - frame) as u32,
                le_u32_at(index, frame + 4),
                count as u32
            ),
            "the hash of the L0 frame at {frame}"
        );
        let mut at = frame + 20;

        for _ in 0..count {
            let (from, after) = key_at(index, at);
            let (to, after) = key_at(index, after);
            intervals.push((from, to, le_u32_at(index, after)));
            at = after + 4;
        }

        assert!(at <= end, "the L0 frame at {frame} overflows");
        assert!(
            at == end || index[at] == 0,
            "the L0 frame at {frame} holds more than {count} intervals"
        );
    }

    let mut pages = Vec::new();
    let mut sizes = Vec::new();

    for (n, (from, to, offset)) in intervals.iter().enumerate() {
        assert!(from <= to, "interval {n} is upside down");
        assert!(
            n == 0 || intervals[n - 1].1 < *from,
            "interval {n} is not after the previous one"
        );

        let at = *offset as usize;
        let size = le_u32_at(index, at) as usize;
        if let Some(expected) = l1_page_bytes {
            assert_eq!(size, expected, "the size of the L1 page at {at}");
        }
        assert_eq!(le_u32_at(index, at + 4), 0, "the L1 page at {at} chains");

        let count = le_u32_at(index, at + 8) as usize;
        assert!(count >= 1, "the L1 page at {at} is empty");

        let mut position = at + 12;
        let entries: Vec<(Vec<u8>, DataRef)> = (0..count)
            .map(|_| {
                let (key, after) = key_at(index, position);
                let data = DataRef {
                    file_no: index[after],
                    offset: le_u32_at(index, after + 1),
                };
                position = after + 5;
                (key, data)
            })
            .collect();

        match l1_page_bytes {
            Some(_) => assert!(
                position < at + size && index[position] == 0 || position == at + size,
                "the entries of the L1 page at {at} do not end at its padding"
            ),
            None => assert_eq!(
                position,
                at + size,
                "a compact L1 page is exactly its entries"
            ),
        }

        assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the keys of the L1 page at {at} do not ascend"
        );
        assert!(
            entries[0].0 == *from && entries[count - 1].0 == *to,
            "the L1 page at {at} does not span its interval"
        );

        pages.push(entries);
        sizes.push(size);
    }

    let offsets: BTreeSet<u32> = intervals.iter().map(|(_, _, offset)| *offset).collect();
    assert_eq!(offsets.len(), intervals.len(), "two intervals share a page");

    ParsedIndex {
        intervals,
        pages,
        sizes,
    }
}

/// The compact split archive of `names`, from the written-down layout with its literal sizes: the index
/// (L0 `20 + Σ (P from · P to · 4)`, L1 pages of `12 + Σ (P key · 5)`, each closed at 1024 keys or before
/// 16 KiB) and the data files of `pages_per_file` pages of `10 + 24 × 128` each.
pub fn reference_split_compact(names: &Names, pages_per_file: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    const MAX_KEYS: usize = 1024;
    const PAGE_BYTES: usize = 16 * 1024;

    let mut pages: Vec<(Vec<&[u8]>, usize)> = Vec::new();

    for key in names.keys() {
        let entry = prefixed(key).len() + 5;

        match pages.last_mut() {
            Some((keys, len)) if keys.len() < MAX_KEYS && *len + entry <= PAGE_BYTES => {
                keys.push(key);
                *len += entry;
            }
            _ => pages.push((vec![key], 12 + entry)),
        }
    }

    let l0_len = 20
        + pages
            .iter()
            .map(|(keys, _)| prefixed(keys[0]).len() + prefixed(keys[keys.len() - 1]).len() + 4)
            .sum::<usize>();

    let mut index = Vec::new();
    index.extend_from_slice(&(l0_len as u32).to_le_bytes());
    index.extend_from_slice(&0u32.to_le_bytes());
    index.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    index.extend_from_slice(&l0_hash(l0_len as u32, 0, pages.len() as u32).to_le_bytes());

    let mut l1_offset = l0_len;
    for (keys, len) in &pages {
        index.extend_from_slice(&prefixed(keys[0]));
        index.extend_from_slice(&prefixed(keys[keys.len() - 1]));
        index.extend_from_slice(&(l1_offset as u32).to_le_bytes());
        l1_offset += len;
    }

    let mut rank = 0;
    for (keys, len) in &pages {
        index.extend_from_slice(&(*len as u32).to_le_bytes());
        index.extend_from_slice(&0u32.to_le_bytes());
        index.extend_from_slice(&(keys.len() as u32).to_le_bytes());

        for key in keys {
            index.extend_from_slice(&prefixed(key));
            index.push((rank / pages_per_file) as u8);
            index.extend_from_slice(&((rank % pages_per_file * DATA_PAGE) as u32).to_le_bytes());
            rank += 1;
        }
    }

    let mut data: Vec<Vec<u8>> = Vec::new();

    for (rank, hours) in names.values().enumerate() {
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

/// The keys of [`hand_built`]: `a`, `bravo` and `c` in interval 0, a 130-byte key of `k` — a two-byte
/// length prefix — in interval 1.
pub const HB_A: &[u8] = b"a";
pub const HB_BRAVO: &[u8] = b"bravo";
pub const HB_C: &[u8] = b"c";

pub fn hb_kilo() -> Vec<u8> {
    vec![b'k'; 130]
}

/// Where each frame of [`hand_built`] lies. The index: L0 frame 0 (padded) → an L1 page chained over two
/// frames, the second at the very end → an L0 continuation (padded) after that first L1 frame → a
/// one-frame L1 page of the long key.
pub const HB_L0_0: usize = 0;
pub const HB_L1_A: usize = 32;
pub const HB_L0_1: usize = 68;
pub const HB_L1_B: usize = 358;
pub const HB_L1_A2: usize = 507;
pub const HB_INDEX_LEN: usize = 539;

/// Data file 0: `a` in one frame, `bravo` in three frames with filler between them, `c` and the long key.
pub const HB_K1: usize = 0;
pub const HB_K2_F1: usize = 3082;
pub const HB_K2_F2: usize = 4100;
pub const HB_K2_F3: usize = 4620;
pub const HB_K3: usize = 6202;
pub const HB_K10: usize = 9284;
pub const HB_DATA_LEN: usize = 12366;

/// An archive no writer produces: chained frames on L0, L1 and data. A key's slots `h` with
/// `h % 5 == key[0] % 5` are written, with `key[0] × 100 + h`.
pub struct HandBuilt {
    pub index: Vec<u8>,
    pub data: Vec<u8>,
    pub names: Names,
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
    let kilo = hb_kilo();
    let mut names: Names = BTreeMap::new();

    for key in [HB_A, HB_BRAVO, HB_C, &kilo] {
        let first = key[0] as u64;
        let hours = (0..24u64)
            .map(|hour| (hour % 5 == first % 5).then(|| Record::with(first * 100 + hour)))
            .collect();
        names.insert(key.to_vec(), hours);
    }

    let body = |key: &[u8]| -> Vec<u8> {
        names[key]
            .iter()
            .flat_map(|hour| hour.clone().unwrap_or_default().0)
            .collect()
    };

    let interval = |from: &[u8], to: &[u8], l1_offset: usize| {
        [
            prefixed(from),
            prefixed(to),
            (l1_offset as u32).to_le_bytes().to_vec(),
        ]
        .concat()
    };

    let entry = |key: &[u8], offset: usize| {
        [
            prefixed(key),
            vec![0],
            (offset as u32).to_le_bytes().to_vec(),
        ]
        .concat()
    };

    let mut index = l0_frame(32, HB_L0_1 as u32, 1, &interval(HB_A, HB_C, HB_L1_A));
    index.extend_from_slice(&l1_frame(
        36,
        HB_L1_A2 as u32,
        2,
        &[entry(HB_A, HB_K1), entry(HB_BRAVO, HB_K2_F1)].concat(),
    ));
    index.extend_from_slice(&l0_frame(290, 0, 1, &interval(&kilo, &kilo, HB_L1_B)));
    index.extend_from_slice(&l1_frame(149, 0, 1, &entry(&kilo, HB_K10)));
    index.extend_from_slice(&l1_frame(32, 0, 1, &entry(HB_C, HB_K3)));
    assert_eq!(index.len(), HB_INDEX_LEN);

    let two = body(HB_BRAVO);
    let mut data = data_frame(3082, 0, 128, &body(HB_A));
    data.extend_from_slice(&data_frame(1010, HB_K2_F2 as u32, 128, &two[..1000]));
    data.resize(HB_K2_F2, 0xCC);
    data.extend_from_slice(&data_frame(510, HB_K2_F3 as u32, 128, &two[1000..1500]));
    data.resize(HB_K2_F3, 0xCC);
    data.extend_from_slice(&data_frame(1582, 0, 128, &two[1500..]));
    data.extend_from_slice(&data_frame(3082, 0, 128, &body(HB_C)));
    data.extend_from_slice(&data_frame(3082, 0, 128, &body(&kilo)));
    assert_eq!(data.len(), HB_DATA_LEN);

    HandBuilt { index, data, names }
}
