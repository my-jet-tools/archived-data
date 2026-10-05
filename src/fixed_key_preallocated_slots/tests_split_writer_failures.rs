//! What a split writer does when a write or a flush fails, is dropped halfway, or is refused: a refusal
//! changes nothing and leaves the writer usable, anything else leaves every later call refused, and no key
//! written before is ever lost from what the writer holds.

use super::split_test_support::{
    DATA_PAGE, Gate, NOT_USABLE, Reader, Writer, drop_when_parked, file_bytes, l0_frame_len,
    l1_page_len, parse_index, refused,
};
use super::test_types::{Hour, Login, Record};
use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
use crate::{
    ArchiveError, FixedKeyPreallocatedSlotsArchiveReader, FixedKeyPreallocatedSlotsArchiveWriter,
    MAX_INDEX_BYTES, PrimaryKeyFixedSize,
};

const KEYS: [u64; 8] = [10, 20, 30, 40, 50, 60, 70, 80];

/// Pages A = [10 20 30 40] and B = [50 60 70 80], flushed, at L1 capacity 4 and an L1 cache of one page.
/// `a_cached` leaves A in the cache and B only in the index; otherwise B is cached, A is in the index, and
/// the data page of 80 is cached and dirty. Writing 25 then halves A, and B has no room for the upper half.
async fn two_full_pages(a_cached: bool, data_cache: usize) -> (InMemoryIo, Writer) {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 8, 4).await.unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(data_cache);

    for login in KEYS {
        writer
            .write(Login(login), Hour(0), &Record::with(login))
            .await
            .unwrap();
    }
    writer.flush().await.unwrap();

    let parsed = parse_index(&file_bytes(&io, FileRole::Index).await, 4);
    assert_eq!(parsed.keys(), [KEYS[..4].to_vec(), KEYS[4..].to_vec()]);

    let (login, hour) = if a_cached { (10, 1) } else { (80, 1) };
    writer
        .write(Login(login), Hour(hour), &Record::with(login + 1000))
        .await
        .unwrap();

    (io, writer)
}

fn page_offset(page: usize) -> u64 {
    (l0_frame_len(8) + page * l1_page_len(4)) as u64
}

/// Every key written before, with what was written to it, and `extra` keys at hour 0.
async fn assert_holds(io: InMemoryIo, a_cached: bool, extra: &[u64]) {
    let reader = Reader::open(io).await.unwrap();
    let (touched, hour) = if a_cached { (10, 1) } else { (80, 1) };

    for login in KEYS.iter().chain(extra) {
        let mut expected = vec![Record::default(); 2];
        expected[0] = Record::with(*login);
        if *login == touched {
            expected[hour] = Record::with(login + 1000);
        }

        assert_eq!(
            reader.get(&Login(*login), Hour(0), Hour(1)).await.unwrap(),
            Some(expected),
            "login {login}"
        );
    }

    assert_eq!(reader.keys_count().await.unwrap(), KEYS.len() + extra.len());
}

/// Every later call refused, and `finish` still finishing every file once.
async fn assert_not_usable(io: &InMemoryIo, mut writer: Writer, case: &str) {
    assert_eq!(
        refused(writer.write(Login(99), Hour(0), &Record::with(99)).await),
        NOT_USABLE,
        "{case}: a new key"
    );
    assert_eq!(
        refused(writer.write(Login(10), Hour(5), &Record::with(1)).await),
        NOT_USABLE,
        "{case}: a known key"
    );
    assert_eq!(refused(writer.flush().await), NOT_USABLE, "{case}: flush");
    assert_eq!(refused(writer.finish().await), NOT_USABLE, "{case}: finish");

    for role in io.roles() {
        let file = io.file(role).unwrap();
        assert_eq!(
            (file.finish_attempts(), file.finishes_spawned_by_drop()),
            (1, 0),
            "{case}: {role:?}"
        );
    }
}

#[tokio::test]
async fn a_split_whose_read_of_the_next_page_fails_loses_no_key_and_poisons_the_writer() {
    for forget in [false, true] {
        let (io, mut writer) = two_full_pages(true, 1024).await;
        let index = io.file(FileRole::Index).unwrap();
        index.fail_reads();

        let reads = index.read_ats();
        let failed = writer.write(Login(25), Hour(0), &Record::with(25)).await;
        assert!(matches!(failed, Err(ArchiveError::Io(_))), "{failed:?}");
        assert_eq!(index.read_ats() - reads, 1, "the one read is page B's");
        assert_eq!((writer.keys_count(), writer.l0_intervals()), (8, 2));

        if !forget {
            assert_not_usable(&io, writer, "a failed read").await;
            continue;
        }

        // Forgiven, the writer finishes what it held when the read failed: nothing was cut.
        writer.forget_in_flight();
        writer.finish().await.unwrap();

        let copy = InMemoryIo::default();
        for role in io.roles() {
            copy.put(role, file_bytes(&io, role).await);
        }
        assert_holds(copy, true, &[]).await;
    }
}

/// Where a dropped write parks: the scenario, the login written, and how many keys the writer counts when
/// the write stops there — 9 only once 25 is in L1.
struct Park {
    case: &'static str,
    a_cached: bool,
    data_cache: usize,
    login: u64,
    keys: usize,
    gate: fn(&InMemoryIo) -> Gate,
}

fn index_read_at(io: &InMemoryIo, page: usize) -> Gate {
    Gate::reads_at(io, FileRole::Index, page_offset(page))
}

fn writes_of(io: &InMemoryIo, role: FileRole) -> Gate {
    Gate::writes_of(io, role)
}

/// The scenario of `park`, with its write dropped where it parks.
async fn dropped_at(park: &Park) -> (InMemoryIo, Writer) {
    let (io, mut writer) = two_full_pages(park.a_cached, park.data_cache).await;
    let gate = (park.gate)(&io);

    drop_when_parked(
        writer.write(Login(park.login), Hour(0), &Record::with(park.login)),
        || gate.is_waiting(),
        park.case,
    )
    .await;
    gate.open();

    assert_eq!(writer.keys_count(), park.keys, "{}", park.case);
    (io, writer)
}

#[tokio::test]
async fn a_write_dropped_at_any_await_refuses_every_later_call() {
    // The first two park before the new key 25 changes anything, the next four after its data page is
    // reserved — the first of them in the spoil of frame 0's hash that precedes the first byte written since
    // the flush — the last in a known key's item.
    let parks = [
        Park {
            case: "reading the key's L1 page",
            a_cached: false,
            data_cache: 1,
            login: 25,
            keys: 8,
            gate: |io| index_read_at(io, 0),
        },
        Park {
            case: "reading the next L1 page",
            a_cached: true,
            data_cache: 1,
            login: 25,
            keys: 8,
            gate: |io| index_read_at(io, 1),
        },
        Park {
            case: "spoiling frame 0's hash before a data page evicted for the key's page",
            a_cached: false,
            data_cache: 1,
            login: 25,
            keys: 8,
            gate: |io| writes_of(io, FileRole::Index),
        },
        Park {
            case: "storing a data page evicted for the key's page",
            a_cached: false,
            data_cache: 1,
            login: 25,
            keys: 8,
            gate: |io| writes_of(io, FileRole::Data(0)),
        },
        Park {
            case: "appending the key's data page",
            a_cached: false,
            data_cache: 0,
            login: 25,
            keys: 8,
            gate: |io| writes_of(io, FileRole::Data(0)),
        },
        Park {
            case: "storing an L1 page evicted once the key is placed",
            a_cached: false,
            data_cache: 1,
            login: 25,
            keys: 9,
            gate: |io| Gate::writes_of_after(io, FileRole::Index, 1),
        },
        Park {
            case: "writing a known key's item straight to its data file",
            a_cached: false,
            data_cache: 0,
            login: 30,
            keys: 8,
            gate: |io| writes_of(io, FileRole::Data(0)),
        },
    ];

    for park in &parks {
        let (io, writer) = dropped_at(park).await;
        assert_not_usable(&io, writer, park.case).await;
    }

    for park in &parks[..2] {
        let (io, mut writer) = dropped_at(park).await;
        let case = park.case;

        // Forgiven, the writer is exactly what it was before the dropped write: 25 goes in as a new key.
        writer.forget_in_flight();
        assert_eq!(writer.keys_count(), 8, "{case}");
        writer
            .write(Login(25), Hour(0), &Record::with(25))
            .await
            .unwrap();
        writer.finish().await.unwrap();

        assert_holds(io, park.a_cached, &[25]).await;
    }
}

#[tokio::test]
async fn a_flush_dropped_halfway_refuses_every_later_call() {
    let parks: [(&str, FileRole); 3] = [
        ("storing the L1 pages", FileRole::Index),
        ("storing the data pages", FileRole::Data(0)),
        ("laying the L0 chain out", FileRole::Index),
    ];

    for (case, role) in parks {
        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), 8, 4).await.unwrap();

        for login in KEYS {
            writer
                .write(Login(login), Hour(0), &Record::with(login))
                .await
                .unwrap();
        }

        if case == "laying the L0 chain out" {
            // Nothing is left in L1 to store, so the first index write of the flush is L0's.
            writer.flush().await.unwrap();
            writer
                .write(Login(10), Hour(1), &Record::with(1))
                .await
                .unwrap();
        }

        let gate = writes_of(&io, role);
        drop_when_parked(writer.flush(), || gate.is_waiting(), case).await;
        gate.open();

        assert_not_usable(&io, writer, case).await;
    }
}

#[tokio::test]
async fn a_new_l1_page_past_the_index_limit_is_refused_before_anything_changes() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 16, 2).await.unwrap();
    writer.set_data_cache_size(1);

    for login in [10, 20, 40] {
        writer
            .write(Login(login), Hour(0), &Record::with(login))
            .await
            .unwrap();
    }

    // Pages [10 20] and [40]: exactly the index there is room for.
    writer.set_max_index_bytes((l0_frame_len(16) + 2 * l1_page_len(2)) as u64);

    for _ in 0..2 {
        // A split whose upper half [20 30] fits neither page B nor a new page.
        let message = refused(writer.write(Login(30), Hour(0), &Record::with(30)).await);
        assert!(message.contains("would grow the index past"), "{message}");
        assert_eq!((writer.keys_count(), writer.l0_intervals()), (3, 2));
    }

    writer.flush().await.unwrap();
    let reader = Reader::open(io.clone()).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), 3);
    for login in [10, 20, 40] {
        assert_eq!(
            reader.get(&Login(login), Hour(0), Hour(0)).await.unwrap(),
            Some(vec![Record::with(login)])
        );
    }

    // Still usable: a key that fits page B, and a known key.
    writer
        .write(Login(50), Hour(0), &Record::with(50))
        .await
        .unwrap();
    writer
        .write(Login(20), Hour(1), &Record::with(21))
        .await
        .unwrap();

    // An append past the last page, and a new page for the first key of an empty writer, alike.
    refused(writer.write(Login(60), Hour(0), &Record::with(60)).await);

    let empty = InMemoryIo::default();
    let mut first = Writer::new_with_capacity(empty.clone(), 16, 2)
        .await
        .unwrap();
    first.set_max_index_bytes(l0_frame_len(16) as u64 + 1);
    refused(first.write(Login(1), Hour(0), &Record::with(1)).await);
    assert_eq!(first.keys_count(), 0);
    first.finish().await.unwrap();
    assert_eq!(empty.roles(), [FileRole::Index], "no data page was placed");

    writer.set_max_index_bytes(MAX_INDEX_BYTES);
    for login in [30, 60] {
        writer
            .write(Login(login), Hour(0), &Record::with(login))
            .await
            .unwrap();
    }
    writer.finish().await.unwrap();

    // The refused keys took no data page: six keys, six pages, ranks without a gap.
    assert_eq!(
        file_bytes(&io, FileRole::Data(0)).await.len(),
        6 * DATA_PAGE
    );
    let mut offsets: Vec<u32> = parse_index(&file_bytes(&io, FileRole::Index).await, 2)
        .refs()
        .iter()
        .map(|at| at.offset)
        .collect();
    offsets.sort_unstable();
    assert_eq!(
        offsets,
        (0..6)
            .map(|rank| (rank * DATA_PAGE) as u32)
            .collect::<Vec<_>>()
    );

    let reader = Reader::open(io).await.unwrap();
    for login in [10, 30, 40, 50, 60] {
        assert_eq!(
            reader.get(&Login(login), Hour(0), Hour(0)).await.unwrap(),
            Some(vec![Record::with(login)])
        );
    }
    assert_eq!(
        reader.get(&Login(20), Hour(0), Hour(1)).await.unwrap(),
        Some(vec![Record::with(20), Record::with(21)])
    );
}

#[tokio::test]
async fn the_index_limit_counts_the_l0_continuations_a_flush_will_need() {
    // One interval per L0 frame and one key per page: every key after the first needs a continuation.
    let frame = l0_frame_len(1) as u64;
    let page = l1_page_len(1) as u64;

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 1, 1).await.unwrap();
    writer
        .write(Login(10), Hour(0), &Record::with(10))
        .await
        .unwrap();

    // Room for the page of key 20, one byte short of the continuation its interval needs.
    writer.set_max_index_bytes(frame + page + page + frame - 1);
    refused(writer.write(Login(20), Hour(0), &Record::with(20)).await);
    writer.flush().await.unwrap();

    writer.set_max_index_bytes(frame + page + page + frame);
    writer
        .write(Login(20), Hour(0), &Record::with(20))
        .await
        .unwrap();
    refused(writer.write(Login(30), Hour(0), &Record::with(30)).await);
    writer.finish().await.unwrap();

    assert_eq!(
        file_bytes(&io, FileRole::Index).await.len() as u64,
        frame + page + page + frame
    );

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.l0_frames(), 2);
    assert_eq!(reader.keys_count().await.unwrap(), 2);
}

/// A key whose `copy_to` writes one byte short for the value 13.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ShortOn13(u64);

impl PrimaryKeyFixedSize for ShortOn13 {
    const LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(u64::from_le_bytes(src.try_into().unwrap()))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        let bytes = self.0.to_le_bytes();
        let len = if self.0 == 13 { 7 } else { 8 };
        out.extend_from_slice(&bytes[..len]);
    }
}

#[tokio::test]
async fn a_key_that_does_not_encode_is_refused_and_the_next_new_key_is_accepted() {
    let io = InMemoryIo::default();
    let mut writer = FixedKeyPreallocatedSlotsArchiveWriter::<
        ShortOn13,
        Hour,
        Record,
        InMemoryIo,
        InMemoryWriter,
        InMemoryReader,
    >::new_with_capacity(io.clone(), 8, 2)
    .await
    .unwrap();

    for login in [12, 13, 14, 13, 15] {
        let result = writer
            .write(ShortOn13(login), Hour(0), &Record::with(login))
            .await;

        if login == 13 {
            assert!(refused(result).contains("copy_to wrote 7 bytes"));
        } else {
            result.unwrap();
        }
    }

    assert_eq!(writer.keys_count(), 3);
    writer.finish().await.unwrap();

    let reader = FixedKeyPreallocatedSlotsArchiveReader::<
        ShortOn13,
        Hour,
        Record,
        InMemoryIo,
        InMemoryReader,
    >::open(io)
    .await
    .unwrap();

    assert_eq!(reader.keys_count().await.unwrap(), 3);
    for login in [12, 14, 15] {
        assert_eq!(
            reader
                .get(&ShortOn13(login), Hour(0), Hour(0))
                .await
                .unwrap(),
            Some(vec![Record::with(login)])
        );
    }
}
