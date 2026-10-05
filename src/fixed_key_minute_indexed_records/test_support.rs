//! What the tests of this module share: the records they write, a model of what an archive must answer,
//! the archives they build over `InMemoryIo`, and a parser that reads an archive straight off its bytes with
//! none of the crate's code, asserting every invariant of the format on the way.

use std::collections::BTreeMap;

use rust_extensions::date_time::DateTimeAsMicroseconds;

use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
use crate::test_items::Login;
use crate::{
    FixedKeyMinuteIndexedRecordsArchiveReader, FixedKeyMinuteIndexedRecordsArchiveWriter, l0_hash,
};

pub use crate::split_test_io::{NOT_USABLE, data_files, data_read_ats, file_bytes, refused};
pub use crate::test_items::{Rng, seeded};
pub(crate) use crate::{le_u32_at, le_u64_at};

pub type Writer =
    FixedKeyMinuteIndexedRecordsArchiveWriter<Login, InMemoryIo, InMemoryWriter, InMemoryReader>;
pub type Reader = FixedKeyMinuteIndexedRecordsArchiveReader<Login, InMemoryIo, InMemoryReader>;

/// 2026-09-15T00:00:00, the day every test archive is of.
pub const DAY: i64 = 1_789_430_400_000_000;
pub const MINUTE: i64 = 60_000_000;
pub const NAME: &str = "mt5-test 2026-09-15";

pub const FRAME: usize = 32_768;
pub const L2: usize = 12_968;
/// `key u64 · l2_offset u32`.
pub const L1_ENTRY: usize = 12;
/// `from u64 · to u64 · l1_offset u32`.
pub const L0_ENTRY: usize = 20;

pub fn day() -> DateTimeAsMicroseconds {
    DateTimeAsMicroseconds::new(DAY)
}

pub fn time(minute: i64, microseconds: i64) -> i64 {
    DAY + minute * MINUTE + microseconds
}

pub fn moment(minute: i64, microseconds: i64) -> DateTimeAsMicroseconds {
    DateTimeAsMicroseconds::new(time(minute, microseconds))
}

/// A record of `len` bytes: its time, its id, then filler derived from the id.
pub fn record(time: i64, id: u64, len: usize) -> Vec<u8> {
    let mut out = time.to_le_bytes().to_vec();
    out.extend_from_slice(&id.to_le_bytes());
    out.resize(len.max(16), (id % 251) as u8);
    out.truncate(len.max(8));
    out
}

/// The id a record of [`record`] carries; `u64::MAX` for one too short to hold it.
pub fn record_id(bytes: &[u8]) -> u64 {
    if bytes.len() < 16 {
        return u64::MAX;
    }

    le_u64_at(bytes, 8)
}

pub fn ids(records: &[Vec<u8>]) -> Vec<u64> {
    records.iter().map(|record| record_id(record)).collect()
}

pub fn record_time_of(bytes: &[u8]) -> i64 {
    le_u64_at(bytes, 0) as i64
}

/// What an archive must answer: every login's records in time order, a record of an equal time after those
/// written before it.
#[derive(Default, Clone)]
pub struct Model {
    pub logins: BTreeMap<u64, Vec<Vec<u8>>>,
}

impl Model {
    pub fn push(&mut self, login: u64, record: &[u8]) {
        let records = self.logins.entry(login).or_default();
        let time = record_time_of(record);
        let at = records.partition_point(|stored| record_time_of(stored) <= time);
        records.insert(at, record.to_vec());
    }

    pub fn of(rows: &[(u64, Vec<u8>)]) -> Self {
        let mut model = Self::default();

        for (login, record) in rows {
            model.push(*login, record);
        }

        model
    }

    pub fn day(&self, login: u64) -> Vec<Vec<u8>> {
        self.logins.get(&login).cloned().unwrap_or_default()
    }

    pub fn range(&self, login: u64, from: i64, to: i64) -> Vec<Vec<u8>> {
        self.day(login)
            .into_iter()
            .filter(|record| (from..=to).contains(&record_time_of(record)))
            .collect()
    }
}

/// `count` records over `logins` logins: times all over the day, a few sharing a time, lengths from 8 bytes
/// to 2 KiB and a few of 8 KiB, fully shuffled — so frames fill, halve and chain in every way.
pub fn random_rows(logins: u64, count: usize, rng: &mut Rng) -> Vec<(u64, Vec<u8>)> {
    let mut rows = Vec::with_capacity(count);

    for id in 0..count as u64 {
        let login = 1_000 + rng.next() % logins * 7;
        let minute = if rng.next().is_multiple_of(3) {
            (rng.next() % 30) as i64
        } else {
            (rng.next() % 1440) as i64
        };
        let micros = if rng.next().is_multiple_of(5) {
            0
        } else {
            (rng.next() % MINUTE as u64) as i64
        };
        let len = match rng.next() % 50 {
            0 => 8 * 1024,
            1 => 8,
            _ => 16 + (rng.next() % 2_000) as usize,
        };

        rows.push((login, record(time(minute, micros), id, len)));
    }

    rows
}

pub async fn write_all(writer: &mut Writer, rows: &[(u64, Vec<u8>)]) {
    for (login, record) in rows {
        writer.write(Login(*login), record).await.unwrap();
    }
}

/// A finished working archive of `rows`, with L1 pages of `capacity_l1` and every cache holding `cache`.
pub async fn build(rows: &[(u64, Vec<u8>)], capacity_l1: usize, cache: usize) -> InMemoryIo {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), NAME, day(), 4, capacity_l1)
        .await
        .unwrap();

    writer.set_l1_cache_size(cache);
    writer.set_l2_cache_size(cache);
    writer.set_data_cache_size(cache);

    write_all(&mut writer, rows).await;
    writer.finish().await.unwrap();
    io
}

pub async fn open(io: &InMemoryIo) -> Reader {
    Reader::open(io.clone(), day()).await.unwrap()
}

/// The compact archive of `reader`, under the reader's `max_data_file_bytes`.
pub async fn compact(reader: &Reader) -> InMemoryIo {
    let out = InMemoryIo::default();
    let plan = reader.compaction_plan().await.unwrap();
    reader.write_compacted_to(&plan, &out).await.unwrap();
    out
}

/// Every login's day read back must be the model's, record for record.
pub async fn assert_reads_back(reader: &Reader, model: &Model, case: &str) {
    for (login, records) in &model.logins {
        let read = reader.get_day(&Login(*login)).await.unwrap();
        assert!(
            read == *records,
            "{case}: login {login} reads back differently"
        );
    }
}

/// One data frame as its bytes hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFrame {
    pub file_no: u8,
    pub offset: u32,
    pub size: u32,
    pub next: u32,
    pub ids: Vec<u64>,
}

/// One key as its bytes hold it: its L2 frame's cells — `(file, frame_start, offset)` or empty — its frames
/// in chain order, and its records.
#[derive(Debug, Clone)]
pub struct ParsedKey {
    pub l2_offset: u32,
    pub cells: Vec<Option<(u8, u32, u32)>>,
    pub frames: Vec<ParsedFrame>,
    pub records: Vec<Vec<u8>>,
}

/// An archive read straight off its bytes, asserting on the way: every L0 header's hash and ordered
/// intervals; every L1 page one frame of strictly ascending keys spanning its interval; every L2 frame 12 968
/// bytes with `next` 0; every empty cell nine `0xFF` bytes; every record's time inside the day and no earlier
/// than the one before; every minute with a record pointed at its first record, and no other minute pointed
/// anywhere; the chain visiting no frame twice.
pub async fn parse_archive(io: &InMemoryIo) -> BTreeMap<u64, ParsedKey> {
    let index = file_bytes(io, FileRole::Index).await;
    let files = data_files(io).await;

    let mut intervals: Vec<(u64, u64, u32)> = Vec::new();
    let mut frame = 0usize;

    loop {
        let size = le_u32_at(&index, frame);
        let next = le_u32_at(&index, frame + 4);
        let count = le_u32_at(&index, frame + 8);
        assert_eq!(
            le_u64_at(&index, frame + 12),
            l0_hash(size, next, count),
            "the hash of the L0 frame at {frame}"
        );
        assert!(20 + count as usize * L0_ENTRY <= size as usize);

        for entry in 0..count as usize {
            let at = frame + 20 + entry * L0_ENTRY;
            intervals.push((
                le_u64_at(&index, at),
                le_u64_at(&index, at + 8),
                le_u32_at(&index, at + 16),
            ));
        }

        if next == 0 {
            break;
        }

        frame = next as usize;
    }

    let mut keys = BTreeMap::new();

    for (n, (from, to, l1_offset)) in intervals.iter().enumerate() {
        assert!(from <= to);
        assert!(
            n == 0 || intervals[n - 1].1 < *from,
            "interval {n} is not ordered"
        );

        let at = *l1_offset as usize;
        assert_eq!(le_u32_at(&index, at + 4), 0, "the L1 page at {at} chains");
        let count = le_u32_at(&index, at + 8) as usize;
        assert!(count > 0 && 12 + count * L1_ENTRY <= le_u32_at(&index, at) as usize);

        let entries: Vec<(u64, u32)> = (0..count)
            .map(|entry| {
                let at = at + 12 + entry * L1_ENTRY;
                (le_u64_at(&index, at), le_u32_at(&index, at + 8))
            })
            .collect();

        assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!((entries[0].0, entries[count - 1].0), (*from, *to));

        for (login, l2_offset) in entries {
            keys.insert(login, parse_key(&index, &files, l2_offset));
        }
    }

    keys
}

fn parse_key(index: &[u8], files: &[Vec<u8>], l2_offset: u32) -> ParsedKey {
    let at = l2_offset as usize;
    assert_eq!(
        le_u32_at(index, at) as usize,
        L2,
        "the size of the L2 frame at {at}"
    );
    assert_eq!(le_u32_at(index, at + 4), 0, "the L2 frame at {at} chains");

    let cells: Vec<Option<(u8, u32, u32)>> = (0..1440)
        .map(|minute| {
            let cell = &index[at + 8 + minute * 9..at + 8 + (minute + 1) * 9];
            let offset = le_u32_at(cell, 5);

            if offset == u32::MAX {
                assert_eq!(cell, &[0xFF; 9], "the empty cell of minute {minute}");
                None
            } else {
                Some((cell[0], le_u32_at(cell, 1), offset))
            }
        })
        .collect();

    let mut frames: Vec<ParsedFrame> = Vec::new();
    let mut records = Vec::new();
    let mut firsts: Vec<Option<(u8, u32, u32)>> = vec![None; 1440];

    let Some(start) = cells.iter().flatten().next().copied() else {
        return ParsedKey {
            l2_offset,
            cells,
            frames,
            records,
        };
    };

    let (file_no, mut offset, mut at) = start;
    let file = &files[file_no as usize];
    let mut last: Option<(i64, usize)> = None;

    loop {
        assert!(
            frames.iter().all(|frame| frame.offset != offset),
            "the chain returns to the frame at {offset}"
        );

        let base = offset as usize;
        let size = le_u32_at(file, base);
        let next = le_u32_at(file, base + 4);
        let used = u16::from_le_bytes(file[base + 8..base + 10].try_into().unwrap()) as usize;
        assert!(10 + used <= size as usize && base + size as usize <= file.len());

        let mut ids = Vec::new();

        while (at as usize) < 10 + used {
            let within = base + at as usize;
            let len = u16::from_le_bytes(file[within..within + 2].try_into().unwrap()) as usize;
            let bytes = file[within + 2..within + 2 + len].to_vec();
            let time = record_time_of(&bytes);

            assert!(
                (DAY..DAY + 1440 * MINUTE).contains(&time),
                "a record outside the day"
            );
            let minute = ((time - DAY) / MINUTE) as usize;

            if let Some((last_time, last_minute)) = last {
                assert!(last_time <= time, "records out of time order");

                if last_minute != minute {
                    firsts[minute] = Some((file_no, offset, at));
                }
            } else {
                firsts[minute] = Some((file_no, offset, at));
            }

            last = Some((time, minute));
            ids.push(record_id(&bytes));
            records.push(bytes);
            at += 2 + len as u32;
        }

        frames.push(ParsedFrame {
            file_no,
            offset,
            size,
            next,
            ids,
        });

        if next == 0 {
            break;
        }

        offset = next;
        at = 10;
    }

    assert_eq!(cells, firsts, "the cells of the L2 frame at {l2_offset}");

    ParsedKey {
        l2_offset,
        cells,
        frames,
        records,
    }
}
