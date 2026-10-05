//! A working index opens only when its last flush COMPLETED and nothing was written after it. A writer that
//! never completed a flush, one that wrote any byte after its last completed flush, and a flush that fails,
//! is dropped or dies at any of its writes, leave an index refused at open as `Corrupted` — never one that
//! opens holding no keys, answers `None` for a key a flush saved, or answers what no flush saved.

use std::collections::BTreeMap;

use super::split_test_support::{
    Gate, NOT_USABLE, Reader, Writer, drop_when_parked, refused, seeded,
};
use super::test_types::{Hour, Login, Record};
use crate::ArchiveError;
use crate::in_memory_io::{FileRole, InMemoryIo};

const FLUSHED: [u64; 4] = [10, 20, 30, 40];

async fn write(writer: &mut Writer, logins: &[u64]) {
    for login in logins {
        writer
            .write(Login(*login), Hour(0), &Record::with(*login))
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

/// `open` must succeed on an archive holding exactly `logins`, each read back.
async fn assert_opens_with(io: &InMemoryIo, logins: &[u64], case: &str) {
    let reader = Reader::open(io.clone())
        .await
        .unwrap_or_else(|error| panic!("{case}: {error}"));

    assert_eq!(reader.keys_count().await.unwrap(), logins.len(), "{case}");

    for login in logins {
        assert_eq!(
            reader.get(&Login(*login), Hour(0), Hour(0)).await.unwrap(),
            Some(vec![Record::with(*login)]),
            "{case}: login {login}"
        );
    }
}

/// The reviewer's scenario: one interval per L1 page and two per L0 frame, 10 · 20 · 30 · 40 flushed, then
/// 15 written — a page of its own and one more interval, which moves every interval after it one place
/// along the L0 chain and takes a continuation the index does not have yet.
async fn flushed_then_one_more() -> (InMemoryIo, Writer) {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 2, 1).await.unwrap();

    write(&mut writer, &FLUSHED).await;
    writer.flush().await.unwrap();
    write(&mut writer, &[15]).await;

    (io, writer)
}

/// How many appends and `write_at`s of the index the flush of [`flushed_then_one_more`] makes, from a flush
/// that completes — which leaves every key readable.
async fn flush_index_writes() -> (usize, usize) {
    let (io, mut writer) = flushed_then_one_more().await;
    let index = io.file(FileRole::Index).unwrap();
    let before = (index.appends(), index.write_ats());

    writer.flush().await.unwrap();
    assert_opens_with(&io, &[10, 15, 20, 30, 40], "a flush that completed").await;

    (index.appends() - before.0, index.write_ats() - before.1)
}

#[tokio::test]
async fn a_writer_that_never_completed_a_flush_leaves_an_index_refused_at_open() {
    let empty = InMemoryIo::default();
    drop(Writer::new(empty.clone()).await.unwrap());
    assert_refused(&empty, "no key, dropped").await;

    // Caches of one page: most L1 and data pages reach their files before the writer is dropped.
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 4, 2).await.unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);

    let logins: Vec<u64> = (1..=100).map(|login| login * 7 % 101).collect();
    write(&mut writer, &logins).await;
    assert!(io.file(FileRole::Index).unwrap().appends() > 10);
    drop(writer);
    assert_refused(&io, "100 keys, dropped").await;

    // The same, finished: one completed flush makes it an archive.
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 4, 2).await.unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);
    write(&mut writer, &logins).await;
    writer.finish().await.unwrap();

    let mut sorted = logins.clone();
    sorted.sort_unstable();
    assert_opens_with(&io, &sorted, "100 keys, finished").await;

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
    // The spoiled hash, the page of 10 and the continuation rewritten, frame 0's entries and header; the
    // page of 15 and a new continuation appended.
    let (appends, write_ats) = flush_index_writes().await;
    assert_eq!((appends, write_ats), (2, 5));

    // `write_at`s of the index failing after `n`. The first is the one that spoils frame 0's hash: when it
    // fails, nothing was written, and the index still opens with what the flush before saved.
    for n in 0..=write_ats {
        let case = format!("the index's write_ats fail after {n} of {write_ats}");
        let (io, mut writer) = flushed_then_one_more().await;
        io.file(FileRole::Index).unwrap().fail_write_ats_after(n);

        let flushed = writer.flush().await;

        if n == write_ats {
            flushed.unwrap();
            assert_opens_with(&io, &[10, 15, 20, 30, 40], &case).await;
            continue;
        }

        assert!(matches!(flushed, Err(ArchiveError::Io(_))), "{case}");
        assert_eq!(refused(writer.finish().await), NOT_USABLE, "{case}");

        if n == 0 {
            assert_opens_with(&io, &FLUSHED, &case).await;
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
    assert_refused(&io, "the data page of 15 fails to append").await;
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
            assert_opens_with(&io, &FLUSHED, &case).await;
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
    // The flush of 15 appends its L1 page, then reserves a continuation and fails to append it.
    let (io, mut writer) = flushed_then_one_more().await;
    let index = io.file(FileRole::Index).unwrap();
    index.fail_appends_after(1);
    assert!(matches!(writer.flush().await, Err(ArchiveError::Io(_))));
    let appended = index.bytes().await.len();

    // Forgiven — which only a test can do — the writer holds a continuation reserved past the index's end.
    // An item of 10 lands in its cached data page and reaches no file.
    writer.forget_in_flight();
    writer
        .write(Login(10), Hour(3), &Record::with(1003))
        .await
        .unwrap();
    let before = (Reached::of(&io), snapshot(&io).await);

    // Refused before the data pages and before L0: nothing reaches a file.
    let message = refused(writer.flush().await);
    let frame = 20 + 2 * 20;
    assert!(
        message.contains(&format!(
            "the index is reserved up to {} and appended only up to {appended}",
            appended + frame
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
    assert_opens_with(&io, &FLUSHED, "15 written, not flushed").await;

    writer.flush().await.unwrap();
    let mut logins = vec![10, 15, 20, 30, 40];
    assert_opens_with(&io, &logins, "15 flushed").await;

    // Keys before, between and after: new pages, and continuations appended and rewritten.
    let more: Vec<u64> = (0..30).map(|n| 5 + n * 3).collect();
    write(&mut writer, &more[..15]).await;
    writer.flush().await.unwrap();
    logins.extend_from_slice(&more[..15]);
    logins.sort_unstable();
    logins.dedup();
    assert_opens_with(&io, &logins, "a second flush").await;

    write(&mut writer, &more[15..]).await;
    writer.finish().await.unwrap();
    logins.extend_from_slice(&more[15..]);
    logins.sort_unstable();
    logins.dedup();

    let reader = Reader::open(io.clone()).await.unwrap();
    assert!(reader.l0_frames() > 10, "{} L0 frames", reader.l0_frames());
    assert_opens_with(&io, &logins, "finished").await;
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

fn record_at(login: u64, hour: usize) -> Record {
    Record::with(login * 100 + hour as u64)
}

/// `open` must succeed on an archive holding exactly the logins of `rows`, each slot of it read back.
async fn assert_opens_with_rows(
    io: &InMemoryIo,
    rows: &BTreeMap<(u64, usize), Record>,
    case: &str,
) {
    let reader = Reader::open(io.clone())
        .await
        .unwrap_or_else(|error| panic!("{case}: {error}"));

    let mut logins: Vec<u64> = rows.keys().map(|(login, _)| *login).collect();
    logins.dedup();
    assert_eq!(reader.keys_count().await.unwrap(), logins.len(), "{case}");

    for ((login, hour), record) in rows {
        assert_eq!(
            reader
                .get(&Login(*login), Hour(*hour), Hour(*hour))
                .await
                .unwrap(),
            Some(vec![record.clone()]),
            "{case}: login {login}, hour {hour}"
        );
    }
}

/// A cycle between two flushes: `flushed` written at hour 0 and flushed, then `written` — each `(login,
/// hour)` — and what of that `reached` the files before the next flush: `(index appends, index write_ats
/// besides the spoil, data appends, data write_ats)`.
struct Cycle {
    case: &'static str,
    capacity_l1: usize,
    l1_cache: usize,
    data_cache: usize,
    flushed: &'static [u64],
    written: &'static [(u64, usize)],
    reached: (usize, usize, usize, usize),
}

const CYCLES: [Cycle; 5] = [
    Cycle {
        // Pages [10 20 30 40] [50]. 15 splits [20 30 40] off onto the next page, and the first page, the one
        // used longest ago, is evicted in place.
        case: "a split whose L1 page is evicted in place",
        capacity_l1: 4,
        l1_cache: 1,
        data_cache: 1024,
        flushed: &[10, 20, 30, 40, 50],
        written: &[(15, 0)],
        reached: (0, 1, 0, 0),
    },
    Cycle {
        // Pages [10 20 30 40] [50 60]. 55 goes into the second page, its data page appended and its item
        // written in place; the item of 10 evicts that L1 page in place.
        case: "a key inside a flushed page's span, its page evicted",
        capacity_l1: 4,
        l1_cache: 1,
        data_cache: 0,
        flushed: &[10, 20, 30, 40, 50, 60],
        written: &[(55, 0), (10, 1)],
        reached: (0, 1, 1, 2),
    },
    Cycle {
        // 50 starts a page past the index's end, which the item of 10 evicts: an append.
        case: "an L1 page appended",
        capacity_l1: 4,
        l1_cache: 1,
        data_cache: 1024,
        flushed: &[10, 20, 30, 40],
        written: &[(50, 0), (10, 1)],
        reached: (1, 0, 0, 0),
    },
    Cycle {
        // A data cache of one page: 60's data page evicts 50's, which is past its file's end.
        case: "a new key's data page appended",
        capacity_l1: 4,
        l1_cache: 64,
        data_cache: 1,
        flushed: &[10, 20, 30, 40],
        written: &[(50, 0), (60, 0)],
        reached: (0, 0, 1, 0),
    },
    Cycle {
        case: "a slot written in place into an uncached data page of a flushed key",
        capacity_l1: 4,
        l1_cache: 64,
        data_cache: 0,
        flushed: &[10, 20, 30, 40],
        written: &[(20, 5)],
        reached: (0, 0, 0, 1),
    },
];

impl Cycle {
    /// The writer with `flushed` flushed, what that left in the files, and the rows it holds.
    async fn flushed(&self) -> (InMemoryIo, Writer, Reached, BTreeMap<(u64, usize), Record>) {
        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), 4, self.capacity_l1)
            .await
            .unwrap();
        writer.set_l1_cache_size(self.l1_cache);
        writer.set_data_cache_size(self.data_cache);

        let mut rows = BTreeMap::new();
        for login in self.flushed {
            writer
                .write(Login(*login), Hour(0), &record_at(*login, 0))
                .await
                .unwrap();
            rows.insert((*login, 0), record_at(*login, 0));
        }
        writer.flush().await.unwrap();

        let reached = Reached::of(&io);
        (io, writer, reached, rows)
    }

    /// [`Self::flushed`], then `written`: what reached the files since the flush must be `reached`, behind
    /// exactly one spoil.
    async fn written(
        &self,
    ) -> (
        InMemoryIo,
        Writer,
        BTreeMap<(u64, usize), Record>,
        BTreeMap<(u64, usize), Record>,
    ) {
        let (io, mut writer, flushed, rows) = self.flushed().await;
        let mut all = rows.clone();

        for (login, hour) in self.written {
            writer
                .write(Login(*login), Hour(*hour), &record_at(*login, *hour))
                .await
                .unwrap();
            all.insert((*login, *hour), record_at(*login, *hour));
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
        for (login, hour) in cycle.written {
            results.push(
                writer
                    .write(Login(*login), Hour(*hour), &record_at(*login, *hour))
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
        .write(Login(30), Hour(7), &record_at(30, 7))
        .await
        .unwrap();
    assert_eq!(Reached::of(&io), before);

    writer.flush().await.unwrap();
    rows.insert((30, 7), record_at(30, 7));
    assert_eq!(Reached::of(&io).since(before).spoils, 1);
    assert_opens_with_rows(&io, &rows, "an item flushed").await;
}

#[tokio::test]
async fn a_cycle_of_many_writes_spoils_frame_0_exactly_once() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 4, 3).await.unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);

    let mut logins: Vec<u64> = (1..=300).map(|login| login * 7).collect();
    seeded(31).shuffle(&mut logins);
    let mut rows = BTreeMap::new();

    for (cycle, chunk) in logins.chunks(100).enumerate() {
        let before = Reached::of(&io);

        for (n, login) in chunk.iter().enumerate() {
            let hour = n % 24;
            writer
                .write(Login(*login), Hour(hour), &record_at(*login, hour))
                .await
                .unwrap();
            rows.insert((*login, hour), record_at(*login, hour));

            // A key of the cycle before, written again.
            if cycle > 0 {
                let known = logins[n * 3 % (cycle * 100)];
                writer
                    .write(Login(known), Hour(23), &record_at(known, 23))
                    .await
                    .unwrap();
                rows.insert((known, 23), record_at(known, 23));
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
