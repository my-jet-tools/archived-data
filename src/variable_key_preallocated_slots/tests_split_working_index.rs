//! A working index opens only when its last flush COMPLETED and nothing was written after it. A writer that
//! never completed a flush, one that wrote any byte after its last completed flush, and a flush that fails,
//! is dropped or dies at any of its writes, leave an index refused at open as `Corrupted` — never one that
//! opens holding no keys, answers `None` for a key a flush saved, or answers what no flush saved.

use std::collections::BTreeMap;

use super::split_test_support::{
    Gate, NOT_USABLE, Reader, Writer, drop_when_parked, numbered_keys, refused, seeded,
};
use super::test_types::{Hour, Name, Record};
use crate::ArchiveError;
use crate::in_memory_io::{FileRole, InMemoryIo};

/// 28 bytes of room on a page: four entries of a one-byte key. 22 bytes on an L0 frame: two intervals of
/// one-byte bounds.
const L0_FRAME: usize = 42;
const L1_PAGE: usize = 40;

/// `a` to `p`: four full pages, whose four intervals take frame 0 and one continuation.
fn flushed() -> Vec<Vec<u8>> {
    (b'a'..=b'p').map(|byte| vec![byte]).collect()
}

/// A value that follows from the key.
fn record_of(key: &[u8]) -> Record {
    Record::with(key.iter().map(|byte| *byte as u64).sum())
}

async fn write(writer: &mut Writer, keys: &[Vec<u8>]) {
    for key in keys {
        writer
            .write(Name(key.clone()), Hour(0), &record_of(key))
            .await
            .unwrap();
    }
}

/// `open` must refuse the archive as `Corrupted`, on frame 0's hash.
async fn assert_refused(io: &InMemoryIo, case: &str) {
    match Reader::open(io.clone()).await {
        Err(ArchiveError::Corrupted(message)) => {
            assert!(
                message.contains("L0 frame at 0 fails its header hash"),
                "{case}: {message}"
            )
        }
        Err(other) => panic!("{case}: {other}"),
        Ok(reader) => panic!(
            "{case}: opened with {:?} keys",
            reader.keys_count().await.map_err(|error| error.to_string())
        ),
    }
}

/// `open` must succeed on an archive holding exactly `keys`, each read back.
async fn assert_opens_with(io: &InMemoryIo, keys: &[Vec<u8>], case: &str) {
    let reader = Reader::open(io.clone())
        .await
        .unwrap_or_else(|error| panic!("{case}: {error}"));

    assert_eq!(reader.keys_count().await.unwrap(), keys.len(), "{case}");

    for key in keys {
        assert_eq!(
            reader
                .get(&Name(key.clone()), Hour(0), Hour(0))
                .await
                .unwrap(),
            Some(vec![record_of(key)]),
            "{case}: key {key:?}"
        );
    }
}

/// `a` to `p` flushed, then `dd` written: past `d`, its entry overflows the page of `a`, and the page of `e`
/// has no room for it either — a page of its own, and a fifth interval, which moves every interval after it
/// one place along the L0 chain and takes a continuation the index does not have yet.
async fn flushed_then_one_more() -> (InMemoryIo, Writer) {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), L0_FRAME, L1_PAGE)
        .await
        .unwrap();

    write(&mut writer, &flushed()).await;
    writer.flush().await.unwrap();
    assert_eq!((writer.l0_intervals(), writer.l0_frames()), (4, 2));
    write(&mut writer, &[b"dd".to_vec()]).await;
    assert_eq!(writer.l0_intervals(), 5);

    (io, writer)
}

fn with_dd() -> Vec<Vec<u8>> {
    let mut keys = flushed();
    keys.push(b"dd".to_vec());
    keys.sort();
    keys
}

/// How many appends and `write_at`s of the index the flush of [`flushed_then_one_more`] makes, from a flush
/// that completes — which leaves every key readable.
async fn flush_index_writes() -> (usize, usize) {
    let (io, mut writer) = flushed_then_one_more().await;
    let index = io.file(FileRole::Index).unwrap();
    let before = (index.appends(), index.write_ats());

    writer.flush().await.unwrap();
    assert_eq!(writer.l0_frames(), 3);
    assert_opens_with(&io, &with_dd(), "a flush that completed").await;

    (index.appends() - before.0, index.write_ats() - before.1)
}

#[tokio::test]
async fn a_writer_that_never_completed_a_flush_leaves_an_index_refused_at_open() {
    let empty = InMemoryIo::default();
    drop(Writer::new(empty.clone()).await.unwrap());
    assert_refused(&empty, "no key, dropped").await;

    // Caches of one page: most L1 and data pages reach their files before the writer is dropped.
    let mut keys = numbered_keys(100);
    seeded(5).shuffle(&mut keys);

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 128, 64)
        .await
        .unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);
    write(&mut writer, &keys).await;
    assert!(io.file(FileRole::Index).unwrap().appends() > 10);
    drop(writer);
    assert_refused(&io, "100 keys, dropped").await;

    // The same, finished: one completed flush makes it an archive.
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 128, 64)
        .await
        .unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);
    write(&mut writer, &keys).await;
    writer.finish().await.unwrap();
    assert_opens_with(&io, &numbered_keys(100), "100 keys, finished").await;

    let empty = InMemoryIo::default();
    Writer::new(empty.clone())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
    assert_opens_with(&empty, &[], "no key, finished").await;
}

#[tokio::test]
async fn a_failed_flush_leaves_an_index_refused_never_one_missing_a_flushed_key() {
    // The spoiled hash, the page of `a` and the continuation rewritten, frame 0's entries and header; the
    // page of `dd` and a new continuation appended.
    let (appends, write_ats) = flush_index_writes().await;
    assert_eq!((appends, write_ats), (2, 5));

    // `write_at`s of the index failing after `n`. The first is the one that spoils frame 0's hash: when it
    // fails, nothing was written, and the index still opens with what the flush before saved.
    for n in 0..=write_ats {
        let case = format!("the index's write_ats fail after {n} of {write_ats}");
        let (io, mut writer) = flushed_then_one_more().await;
        io.file(FileRole::Index).unwrap().fail_write_ats_after(n);

        let flushed_again = writer.flush().await;

        if n == write_ats {
            flushed_again.unwrap();
            assert_opens_with(&io, &with_dd(), &case).await;
            continue;
        }

        assert!(matches!(flushed_again, Err(ArchiveError::Io(_))), "{case}");
        assert_eq!(refused(writer.finish().await), NOT_USABLE, "{case}");

        if n == 0 {
            assert_opens_with(&io, &flushed(), &case).await;
        } else {
            assert_refused(&io, &case).await;
        }
    }

    // Appends of the index, and the data file's writes, all come after the hash is spoiled.
    for n in 0..appends {
        let case = format!("the index's appends fail after {n} of {appends}");
        let (io, mut writer) = flushed_then_one_more().await;
        io.file(FileRole::Index).unwrap().fail_appends_after(n);

        assert!(writer.flush().await.is_err(), "{case}");
        assert_refused(&io, &case).await;
    }

    let (io, mut writer) = flushed_then_one_more().await;
    io.file(FileRole::Data(0)).unwrap().fail_appends();
    assert!(writer.flush().await.is_err());
    assert_refused(&io, "the data page of dd fails to append").await;
}

#[tokio::test]
async fn a_flush_dropped_at_any_write_leaves_an_index_refused() {
    let (appends, write_ats) = flush_index_writes().await;

    // Parked at its `k`-th write of the index — counting appends and `write_at`s together — and dropped.
    for k in 0..appends + write_ats {
        let case = format!("dropped at index write {k} of {}", appends + write_ats);
        let (io, mut writer) = flushed_then_one_more().await;
        let gate = Gate::writes_of_after(&io, FileRole::Index, k);

        drop_when_parked(writer.flush(), || gate.is_waiting(), &case).await;
        gate.open();
        assert_eq!(refused(writer.finish().await), NOT_USABLE, "{case}");

        if k == 0 {
            // Parked before the hash was spoiled: nothing was changed.
            assert_opens_with(&io, &flushed(), &case).await;
        } else {
            assert_refused(&io, &case).await;
        }
    }

    let (io, mut writer) = flushed_then_one_more().await;
    let gate = Gate::writes_of(&io, FileRole::Data(0));
    drop_when_parked(writer.flush(), || gate.is_waiting(), "the data pages").await;
    gate.open();
    drop(writer);
    assert_refused(&io, "dropped storing the data pages").await;
}

#[tokio::test]
async fn a_flush_refuses_an_index_not_appended_up_to_what_it_reserved() {
    // The flush of `dd` appends its L1 page, then reserves a continuation and fails to append it.
    let (io, mut writer) = flushed_then_one_more().await;
    let index = io.file(FileRole::Index).unwrap();
    index.fail_appends_after(1);
    assert!(matches!(writer.flush().await, Err(ArchiveError::Io(_))));
    let appended = index.bytes().await.len();

    // Forgiven — which only a test can do — the writer holds a continuation reserved past the index's end.
    // An item of `a` lands in its cached data page and reaches no file.
    writer.forget_in_flight();
    writer
        .write(Name(b"a".to_vec()), Hour(3), &Record::with(1003))
        .await
        .unwrap();
    let before = (Reached::of(&io), snapshot(&io).await);

    // Refused before the data pages and before L0: nothing reaches a file.
    let message = refused(writer.flush().await);
    assert!(
        message.contains(&format!(
            "the index is reserved up to {} and appended only up to {appended}",
            appended + L0_FRAME
        )),
        "{message}"
    );
    assert_eq!((Reached::of(&io), snapshot(&io).await), before);

    assert_eq!(refused(writer.finish().await), NOT_USABLE);
    assert_refused(&io, "a flush refused on the frontier").await;
}

#[tokio::test]
async fn every_completed_flush_and_finish_opens_with_every_key_written_before_it() {
    let (io, mut writer) = flushed_then_one_more().await;
    assert_opens_with(&io, &flushed(), "dd written, not flushed").await;

    writer.flush().await.unwrap();
    let mut keys = with_dd();
    assert_opens_with(&io, &keys, "dd flushed").await;

    // Keys before, between and after, of one and two bytes: new pages and splits, continuations appended
    // and rewritten, and bounds that grow longer.
    let mut more: Vec<Vec<u8>> = (0..30u8)
        .map(|n| match n % 3 {
            0 => vec![b'A' + n],
            1 => vec![b'b' + n % 20, b'm'],
            _ => vec![b'q' + n % 9],
        })
        .collect();
    more.sort();
    more.dedup();
    seeded(12).shuffle(&mut more);

    write(&mut writer, &more[..15]).await;
    writer.flush().await.unwrap();
    keys.extend_from_slice(&more[..15]);
    keys.sort();
    keys.dedup();
    assert_opens_with(&io, &keys, "a second flush").await;

    write(&mut writer, &more[15..]).await;
    writer.finish().await.unwrap();
    keys.extend_from_slice(&more[15..]);
    keys.sort();
    keys.dedup();

    let reader = Reader::open(io.clone()).await.unwrap();
    assert!(reader.l0_frames() > 5, "{} L0 frames", reader.l0_frames());
    assert_opens_with(&io, &keys, "finished").await;
}

/// Where frame 0's hash is, and how long: the one `write_at` that spoils it.
const SPOIL: (u64, usize) = (12, 8);

/// What reached the files so far: the index's appends, its `write_at`s besides the spoils, the spoils, and
/// the appends and `write_at`s of every data file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reached {
    index_appends: usize,
    index_write_ats: usize,
    spoils: usize,
    data_appends: usize,
    data_write_ats: usize,
}

impl Reached {
    fn of(io: &InMemoryIo) -> Self {
        let index = io.file(FileRole::Index).unwrap();
        let spoils = index.write_ats_of(SPOIL.0, SPOIL.1);
        let data = io
            .roles()
            .into_iter()
            .filter(|role| *role != FileRole::Index);

        let (data_appends, data_write_ats) = data.fold((0, 0), |(appends, write_ats), role| {
            let file = io.file(role).unwrap();
            (appends + file.appends(), write_ats + file.write_ats())
        });

        Self {
            index_appends: index.appends(),
            index_write_ats: index.write_ats() - spoils,
            spoils,
            data_appends,
            data_write_ats,
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            index_appends: self.index_appends - before.index_appends,
            index_write_ats: self.index_write_ats - before.index_write_ats,
            spoils: self.spoils - before.spoils,
            data_appends: self.data_appends - before.data_appends,
            data_write_ats: self.data_write_ats - before.data_write_ats,
        }
    }

    /// `(index appends, index write_ats, data appends, data write_ats)`, with `spoils` spoils.
    fn writes(writes: (usize, usize, usize, usize), spoils: usize) -> Self {
        Self {
            index_appends: writes.0,
            index_write_ats: writes.1,
            spoils,
            data_appends: writes.2,
            data_write_ats: writes.3,
        }
    }
}

/// Every file's bytes.
async fn snapshot(io: &InMemoryIo) -> Vec<(FileRole, Vec<u8>)> {
    let mut files = Vec::new();

    for role in io.roles() {
        files.push((role, io.file(role).unwrap().bytes().await));
    }

    files
}

type Rows = BTreeMap<(Vec<u8>, usize), Record>;

/// A value that follows from the key and the hour, and tells every key of these tests from every other.
fn record_at(key: &[u8], hour: usize) -> Record {
    let key = key.iter().fold(0u64, |folded, byte| {
        (folded * 257 + *byte as u64) % 1_000_000_007
    });
    Record::with(key * 100 + hour as u64)
}

/// `open` must succeed on an archive holding exactly the keys of `rows`, each slot of it read back.
async fn assert_opens_with_rows(io: &InMemoryIo, rows: &Rows, case: &str) {
    let reader = Reader::open(io.clone())
        .await
        .unwrap_or_else(|error| panic!("{case}: {error}"));

    let mut keys: Vec<&Vec<u8>> = rows.keys().map(|(key, _)| key).collect();
    keys.dedup();
    assert_eq!(reader.keys_count().await.unwrap(), keys.len(), "{case}");

    for ((key, hour), record) in rows {
        assert_eq!(
            reader
                .get(&Name(key.clone()), Hour(*hour), Hour(*hour))
                .await
                .unwrap(),
            Some(vec![record.clone()]),
            "{case}: key {key:?}, hour {hour}"
        );
    }
}

/// A cycle between two flushes: the one-byte keys of `flushed` written at hour 0 and flushed, then
/// `written` — each `(key, hour)` — and what of that `reached` the files before the next flush: `(index
/// appends, index write_ats besides the spoil, data appends, data write_ats)`. Pages hold four one-byte keys.
struct Cycle {
    case: &'static str,
    l1_cache: usize,
    data_cache: usize,
    flushed: &'static [u8],
    written: &'static [(u8, usize)],
    reached: (usize, usize, usize, usize),
}

const CYCLES: [Cycle; 5] = [
    Cycle {
        // Pages [b d f h] [j]. `c` splits `h` off onto the next page, and the first page, the one used longest
        // ago, is evicted in place.
        case: "a split whose L1 page is evicted in place",
        l1_cache: 1,
        data_cache: 1024,
        flushed: b"bdfhj",
        written: &[(b'c', 0)],
        reached: (0, 1, 0, 0),
    },
    Cycle {
        // Pages [b d f h] [j l]. `k` goes into the second page, its data page appended and its item written in
        // place; the item of `b` evicts that L1 page in place.
        case: "a key inside a flushed page's span, its page evicted",
        l1_cache: 1,
        data_cache: 0,
        flushed: b"bdfhjl",
        written: &[(b'k', 0), (b'b', 1)],
        reached: (0, 1, 1, 2),
    },
    Cycle {
        // `j` starts a page past the index's end, which the item of `b` evicts: an append.
        case: "an L1 page appended",
        l1_cache: 1,
        data_cache: 1024,
        flushed: b"bdfh",
        written: &[(b'j', 0), (b'b', 1)],
        reached: (1, 0, 0, 0),
    },
    Cycle {
        // A data cache of one page: `l`'s data page evicts `j`'s, which is past its file's end.
        case: "a new key's data page appended",
        l1_cache: 64,
        data_cache: 1,
        flushed: b"bdfh",
        written: &[(b'j', 0), (b'l', 0)],
        reached: (0, 0, 1, 0),
    },
    Cycle {
        case: "a slot written in place into an uncached data page of a flushed key",
        l1_cache: 64,
        data_cache: 0,
        flushed: b"bdfh",
        written: &[(b'd', 5)],
        reached: (0, 0, 0, 1),
    },
];

impl Cycle {
    /// The writer with `flushed` flushed, what that left in the files, and the rows it holds.
    async fn flushed(&self) -> (InMemoryIo, Writer, Reached, Rows) {
        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), L0_FRAME, L1_PAGE)
            .await
            .unwrap();
        writer.set_l1_cache_size(self.l1_cache);
        writer.set_data_cache_size(self.data_cache);

        let mut rows = Rows::new();
        for key in self.flushed {
            let key = vec![*key];
            writer
                .write(Name(key.clone()), Hour(0), &record_at(&key, 0))
                .await
                .unwrap();
            rows.insert((key.clone(), 0), record_at(&key, 0));
        }
        writer.flush().await.unwrap();

        let reached = Reached::of(&io);
        (io, writer, reached, rows)
    }

    /// [`Self::flushed`], then `written`: what reached the files since the flush must be `reached`, behind
    /// exactly one spoil.
    async fn written(&self) -> (InMemoryIo, Writer, Rows, Rows) {
        let (io, mut writer, flushed, rows) = self.flushed().await;
        let mut all = rows.clone();

        for (key, hour) in self.written {
            let key = vec![*key];
            writer
                .write(Name(key.clone()), Hour(*hour), &record_at(&key, *hour))
                .await
                .unwrap();
            all.insert((key.clone(), *hour), record_at(&key, *hour));
        }

        assert_eq!(
            Reached::of(&io).since(flushed),
            Reached::writes(self.reached, 1),
            "{}",
            self.case
        );

        (io, writer, rows, all)
    }
}

#[tokio::test]
async fn a_writer_dropped_after_writing_anything_since_its_last_flush_leaves_an_index_refused() {
    for cycle in &CYCLES {
        let case = cycle.case;
        let (io, writer, _, _) = cycle.flushed().await;
        let header = io.file(FileRole::Index).unwrap().bytes().await[..20].to_vec();
        drop(writer);

        let (io, writer, _, _) = cycle.written().await;
        drop(writer);
        assert_refused(&io, case).await;

        // The spoil is frame 0's hash inverted, and the only change to its header.
        let spoiled = io.file(FileRole::Index).unwrap().bytes().await[..20].to_vec();
        assert_eq!(spoiled[..12], header[..12], "{case}");
        let inverted: Vec<u8> = header[12..].iter().map(|byte| !byte).collect();
        assert_eq!(spoiled[12..], inverted, "{case}");
    }
}

#[tokio::test]
async fn the_spoil_lands_before_the_first_byte_of_a_cycle_and_a_failed_one_changes_nothing() {
    for cycle in &CYCLES {
        let case = cycle.case;
        let (io, mut writer, before, rows) = cycle.flushed().await;
        let files = snapshot(&io).await;

        io.file(FileRole::Index).unwrap().fail_write_ats();

        let mut results = Vec::new();
        for (key, hour) in cycle.written {
            let key = vec![*key];
            results.push(
                writer
                    .write(Name(key.clone()), Hour(*hour), &record_at(&key, *hour))
                    .await,
            );
        }

        let failed = results.iter().position(Result::is_err);
        assert!(
            matches!(results[failed.expect(case)], Err(ArchiveError::Io(_))),
            "{case}: {results:?}"
        );
        assert!(
            results[failed.unwrap() + 1..]
                .iter()
                .all(|result| matches!(result, Err(ArchiveError::ContractViolation(_)))),
            "{case}: {results:?}"
        );

        // Nothing but the spoil was attempted: no page, no item reached a file.
        let after = Reached::of(&io).since(before);
        assert_eq!(after, Reached::writes((0, 0, 0, 0), 1), "{case}");
        assert_eq!(snapshot(&io).await, files, "{case}");

        drop(writer);
        assert_opens_with_rows(&io, &rows, case).await;
    }
}

#[tokio::test]
async fn a_flush_after_writes_opens_with_every_key_and_spoils_nothing_more() {
    for cycle in &CYCLES {
        let case = cycle.case;
        let (io, mut writer, _, rows) = cycle.written().await;
        let spoils = Reached::of(&io).spoils;

        writer.flush().await.unwrap();
        assert_eq!(Reached::of(&io).spoils, spoils, "{case}");
        assert_opens_with_rows(&io, &rows, case).await;
    }
}

#[tokio::test]
async fn a_flush_with_nothing_written_since_the_last_one_writes_nothing_at_all() {
    for cycle in &CYCLES {
        let case = cycle.case;
        let (io, mut writer, _, rows) = cycle.written().await;
        writer.flush().await.unwrap();

        let flushed = Reached::of(&io);
        let files = snapshot(&io).await;

        writer.flush().await.unwrap();
        writer.flush().await.unwrap();
        assert_eq!(Reached::of(&io), flushed, "{case}: a flush");
        writer.finish().await.unwrap();
        assert_eq!(Reached::of(&io), flushed, "{case}: finish");

        assert_eq!(snapshot(&io).await, files, "{case}");
        assert_opens_with_rows(&io, &rows, case).await;
    }

    // A known key's item into a cached data page reaches no file until the flush, which spoils once for it.
    let (io, mut writer, before, mut rows) = CYCLES[0].flushed().await;
    writer
        .write(Name(b"f".to_vec()), Hour(7), &record_at(b"f", 7))
        .await
        .unwrap();
    assert_eq!(Reached::of(&io), before);

    writer.flush().await.unwrap();
    rows.insert((b"f".to_vec(), 7), record_at(b"f", 7));
    assert_eq!(Reached::of(&io).since(before).spoils, 1);
    assert_opens_with_rows(&io, &rows, "an item flushed").await;
}

#[tokio::test]
async fn a_cycle_of_many_writes_spoils_frame_0_exactly_once() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 128, 64)
        .await
        .unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);

    let mut keys = numbered_keys(300);
    seeded(31).shuffle(&mut keys);
    let mut rows = Rows::new();

    for (cycle, chunk) in keys.chunks(100).enumerate() {
        let before = Reached::of(&io);

        for (n, key) in chunk.iter().enumerate() {
            let hour = n % 24;
            writer
                .write(Name(key.clone()), Hour(hour), &record_at(key, hour))
                .await
                .unwrap();
            rows.insert((key.clone(), hour), record_at(key, hour));

            // A key of the cycle before, written again.
            if cycle > 0 {
                let known = &keys[n * 3 % (cycle * 100)];
                writer
                    .write(Name(known.clone()), Hour(23), &record_at(known, 23))
                    .await
                    .unwrap();
                rows.insert((known.clone(), 23), record_at(known, 23));
            }
        }

        // The index is created with a hash that does not verify: the cycle before the first flush has
        // nothing to spoil.
        let spoils = usize::from(cycle > 0);

        let written = Reached::of(&io).since(before);
        let writes = written.index_appends
            + written.index_write_ats
            + written.data_appends
            + written.data_write_ats;
        assert!(writes > 100, "cycle {cycle}: {written:?}");
        assert_eq!(written.spoils, spoils, "cycle {cycle}");
        assert_refused(&io, &format!("cycle {cycle}, written")).await;

        writer.flush().await.unwrap();
        assert_eq!(
            Reached::of(&io).since(before).spoils,
            spoils,
            "cycle {cycle}"
        );
        assert_opens_with_rows(&io, &rows, &format!("cycle {cycle}, flushed")).await;
    }
}
