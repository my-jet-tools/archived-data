//! What a split writer does when a write or a flush fails, is dropped halfway, or is refused: a refusal
//! changes nothing and leaves the writer usable, anything else leaves every later call refused, and no key
//! written before is ever lost from what the writer holds.

use super::split_test_support::{
    DATA_PAGE, Gate, NOT_USABLE, Reader, Writer, drop_when_parked, file_bytes, parse_index,
    refused, seeded,
};
use super::test_types::{Hour, Name, Record};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::{ArchiveError, MAX_INDEX_BYTES};

const KEYS: [u64; 8] = [10, 20, 30, 40, 50, 60, 70, 80];

/// Four bytes, so an L1 entry is `1 + 4 + 5 = 10` bytes, and the numbers sort as the bytes do.
fn short(n: u64) -> Vec<u8> {
    format!("{n:04}").into_bytes()
}

fn name(n: u64) -> Name {
    Name(short(n))
}

const L0_FRAME: usize = 256;
/// 40 bytes of room: four entries of a four-byte key.
const L1_PAGE: usize = 52;

/// Pages A = [10 20 30 40] and B = [50 60 70 80], flushed, and an L1 cache of one page. `a_cached` leaves A
/// in the cache and B only in the index; otherwise B is cached, A is in the index, and the data page of 80
/// is cached and dirty. Writing 25 then splits 40 off A, and B has no room for it.
async fn two_full_pages(a_cached: bool, data_cache: usize) -> (InMemoryIo, Writer) {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), L0_FRAME, L1_PAGE)
        .await
        .unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(data_cache);

    for n in KEYS {
        writer
            .write(name(n), Hour(0), &Record::with(n))
            .await
            .unwrap();
    }
    writer.flush().await.unwrap();

    let parsed = parse_index(&file_bytes(&io, FileRole::Index).await, Some(L1_PAGE));
    let pages: Vec<Vec<Vec<u8>>> = [&KEYS[..4], &KEYS[4..]]
        .iter()
        .map(|page| page.iter().copied().map(short).collect())
        .collect();
    assert_eq!(parsed.keys(), pages);

    let touched = if a_cached { 10 } else { 80 };
    writer
        .write(name(touched), Hour(1), &Record::with(touched + 1000))
        .await
        .unwrap();

    (io, writer)
}

fn page_offset(page: usize) -> u64 {
    (L0_FRAME + page * L1_PAGE) as u64
}

/// Every key written before, with what was written to it, and `extra` keys at hour 0.
async fn assert_holds(io: InMemoryIo, a_cached: bool, extra: &[u64]) {
    let reader = Reader::open(io).await.unwrap();
    let touched = if a_cached { 10 } else { 80 };

    for n in KEYS.iter().chain(extra) {
        let mut expected = vec![Record::default(); 2];
        expected[0] = Record::with(*n);
        if *n == touched {
            expected[1] = Record::with(n + 1000);
        }

        assert_eq!(
            reader.get(&name(*n), Hour(0), Hour(1)).await.unwrap(),
            Some(expected),
            "key {n}"
        );
    }

    assert_eq!(reader.keys_count().await.unwrap(), KEYS.len() + extra.len());
}

/// Every later call refused, and `finish` still finishing every file once.
async fn assert_not_usable(io: &InMemoryIo, mut writer: Writer, case: &str) {
    assert_eq!(
        refused(writer.write(name(99), Hour(0), &Record::with(99)).await),
        NOT_USABLE,
        "{case}: a new key"
    );
    assert_eq!(
        refused(writer.write(name(10), Hour(5), &Record::with(1)).await),
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

/// A byte-for-byte copy of every file, so a gate or a failure set on the source reaches no reader.
async fn copy_of(io: &InMemoryIo) -> InMemoryIo {
    let copy = InMemoryIo::default();
    for role in io.roles() {
        copy.put(role, file_bytes(io, role).await);
    }
    copy
}

#[tokio::test]
async fn a_split_whose_read_of_the_next_page_fails_loses_no_key_and_poisons_the_writer() {
    for forget in [false, true] {
        let (io, mut writer) = two_full_pages(true, 1024).await;
        let index = io.file(FileRole::Index).unwrap();
        index.fail_reads();

        let reads = index.read_ats();
        let failed = writer.write(name(25), Hour(0), &Record::with(25)).await;
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
        assert_holds(copy_of(&io).await, true, &[]).await;
    }
}

/// Where a dropped write parks: the scenario, the key written, and how many keys the writer counts when
/// the write stops there — 9 only once 25 is in L1.
struct Park {
    case: &'static str,
    a_cached: bool,
    data_cache: usize,
    key: u64,
    keys: usize,
    gate: fn(&InMemoryIo) -> Gate,
}

/// The scenario of `park`, with its write dropped where it parks.
async fn dropped_at(park: &Park) -> (InMemoryIo, Writer) {
    let (io, mut writer) = two_full_pages(park.a_cached, park.data_cache).await;
    let gate = (park.gate)(&io);

    drop_when_parked(
        writer.write(name(park.key), Hour(0), &Record::with(park.key)),
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
            key: 25,
            keys: 8,
            gate: |io| Gate::reads_at(io, FileRole::Index, page_offset(0)),
        },
        Park {
            case: "reading the next L1 page",
            a_cached: true,
            data_cache: 1,
            key: 25,
            keys: 8,
            gate: |io| Gate::reads_at(io, FileRole::Index, page_offset(1)),
        },
        Park {
            case: "spoiling frame 0's hash before a data page evicted for the key's page",
            a_cached: false,
            data_cache: 1,
            key: 25,
            keys: 8,
            gate: |io| Gate::writes_of(io, FileRole::Index),
        },
        Park {
            case: "storing a data page evicted for the key's page",
            a_cached: false,
            data_cache: 1,
            key: 25,
            keys: 8,
            gate: |io| Gate::writes_of(io, FileRole::Data(0)),
        },
        Park {
            case: "appending the key's data page",
            a_cached: false,
            data_cache: 0,
            key: 25,
            keys: 8,
            gate: |io| Gate::writes_of(io, FileRole::Data(0)),
        },
        Park {
            case: "storing an L1 page evicted once the key is placed",
            a_cached: false,
            data_cache: 1,
            key: 25,
            keys: 9,
            gate: |io| Gate::writes_of_after(io, FileRole::Index, 1),
        },
        Park {
            case: "writing a known key's item straight to its data file",
            a_cached: false,
            data_cache: 0,
            key: 30,
            keys: 8,
            gate: |io| Gate::writes_of(io, FileRole::Data(0)),
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
            .write(name(25), Hour(0), &Record::with(25))
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
        let mut writer = Writer::new_with_capacity(io.clone(), L0_FRAME, L1_PAGE)
            .await
            .unwrap();

        for n in KEYS {
            writer
                .write(name(n), Hour(0), &Record::with(n))
                .await
                .unwrap();
        }

        if case == "laying the L0 chain out" {
            // Nothing is left in L1 to store, so the first index write of the flush is L0's.
            writer.flush().await.unwrap();
            writer
                .write(name(10), Hour(1), &Record::with(1))
                .await
                .unwrap();
        }

        let gate = Gate::writes_of(&io, role);
        drop_when_parked(writer.flush(), || gate.is_waiting(), case).await;
        gate.open();

        assert_not_usable(&io, writer, case).await;
    }
}

#[tokio::test]
async fn a_new_l1_page_past_the_index_limit_is_refused_before_anything_changes() {
    // 20 bytes of room: two entries of a four-byte key.
    const PAGE: usize = 32;

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), L0_FRAME, PAGE)
        .await
        .unwrap();
    writer.set_data_cache_size(1);

    for n in [10, 20, 40, 50, 60] {
        writer
            .write(name(n), Hour(0), &Record::with(n))
            .await
            .unwrap();
    }

    // Pages [10 20], [40 50] and [60]: exactly the index there is room for.
    writer.set_max_index_bytes((L0_FRAME + 3 * PAGE) as u64);

    for _ in 0..2 {
        // A split whose upper part [30] fits neither page B nor a new page.
        let message = refused(writer.write(name(30), Hour(0), &Record::with(30)).await);
        assert!(message.contains("would grow the index past"), "{message}");
        assert_eq!((writer.keys_count(), writer.l0_intervals()), (5, 3));
    }

    writer.flush().await.unwrap();
    let reader = Reader::open(io.clone()).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), 5);
    for n in [10, 20, 40, 50, 60] {
        assert_eq!(
            reader.get(&name(n), Hour(0), Hour(0)).await.unwrap(),
            Some(vec![Record::with(n)])
        );
    }

    // Still usable: a key that fits page C, and a known key.
    writer
        .write(name(70), Hour(0), &Record::with(70))
        .await
        .unwrap();
    writer
        .write(name(20), Hour(1), &Record::with(21))
        .await
        .unwrap();

    // An append past the last page, and a new page for the first key of an empty writer, alike.
    refused(writer.write(name(80), Hour(0), &Record::with(80)).await);

    let empty = InMemoryIo::default();
    let mut first = Writer::new_with_capacity(empty.clone(), L0_FRAME, PAGE)
        .await
        .unwrap();
    first.set_max_index_bytes(L0_FRAME as u64 + 1);
    refused(first.write(name(1), Hour(0), &Record::with(1)).await);
    assert_eq!(first.keys_count(), 0);
    first.finish().await.unwrap();
    assert_eq!(empty.roles(), [FileRole::Index], "no data page was placed");

    writer.set_max_index_bytes(MAX_INDEX_BYTES);
    for n in [30, 80] {
        writer
            .write(name(n), Hour(0), &Record::with(n))
            .await
            .unwrap();
    }
    writer.finish().await.unwrap();

    // The refused keys took no data page: eight keys, eight pages, ranks without a gap.
    assert_eq!(
        file_bytes(&io, FileRole::Data(0)).await.len(),
        8 * DATA_PAGE
    );
    let mut offsets: Vec<u32> = parse_index(&file_bytes(&io, FileRole::Index).await, Some(PAGE))
        .refs()
        .iter()
        .map(|at| at.offset)
        .collect();
    offsets.sort_unstable();
    assert_eq!(
        offsets,
        (0..8)
            .map(|rank| (rank * DATA_PAGE) as u32)
            .collect::<Vec<_>>()
    );

    let reader = Reader::open(io).await.unwrap();
    for n in [10, 30, 40, 50, 60, 70, 80] {
        assert_eq!(
            reader.get(&name(n), Hour(0), Hour(0)).await.unwrap(),
            Some(vec![Record::with(n)])
        );
    }
    assert_eq!(
        reader.get(&name(20), Hour(0), Hour(1)).await.unwrap(),
        Some(vec![Record::with(20), Record::with(21)])
    );
}

async fn write_byte_keys(writer: &mut Writer, keys: &[&[u8]]) -> Vec<Result<(), ArchiveError>> {
    let mut results = Vec::new();

    for key in keys {
        results.push(
            writer
                .write(Name(key.to_vec()), Hour(0), &Record::with(key.len() as u64))
                .await,
        );
    }

    results
}

#[tokio::test]
async fn the_index_limit_counts_the_l0_continuations_a_flush_will_need() {
    // The smallest sizes: a page holds two one-byte keys, a frame one interval of them. So every page
    // after the first needs a continuation.
    const FRAME: u64 = 28;
    const PAGE: u64 = 26;

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), FRAME as usize, PAGE as usize)
        .await
        .unwrap();
    for result in write_byte_keys(&mut writer, &[b"a", b"b"]).await {
        result.unwrap();
    }

    // Room for the page of key c, one byte short of the continuation its interval needs.
    writer.set_max_index_bytes(FRAME + PAGE + PAGE + FRAME - 1);
    refused(write_byte_keys(&mut writer, &[b"c"]).await.remove(0));
    writer.flush().await.unwrap();

    writer.set_max_index_bytes(FRAME + PAGE + PAGE + FRAME);
    let results = write_byte_keys(&mut writer, &[b"c", b"d", b"e"]).await;
    let [c, d, e] = <[_; 3]>::try_from(results).unwrap();
    c.unwrap();
    d.unwrap();
    refused(e);
    writer.finish().await.unwrap();

    assert_eq!(
        file_bytes(&io, FileRole::Index).await.len() as u64,
        FRAME + PAGE + PAGE + FRAME
    );

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.l0_frames(), 2);
    assert_eq!(reader.keys_count().await.unwrap(), 4);
}

#[tokio::test]
async fn a_longer_key_that_needs_a_continuation_is_refused_though_it_needs_no_page() {
    // 28 bytes of room on a page, 22 on an L0 frame: [a .. d] and [e .. e] are 8 bytes of L0 each and
    // share frame 0, and [e .. ezzzzzzz] is 15 and does not fit beside [a .. d].
    const FRAME: u64 = 42;
    const PAGE: u64 = 40;
    let long: &[u8] = b"ezzzzzzz";

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), FRAME as usize, PAGE as usize)
        .await
        .unwrap();
    for result in write_byte_keys(&mut writer, &[b"a", b"b", b"c", b"d", b"e"]).await {
        result.unwrap();
    }
    writer.flush().await.unwrap();
    assert_eq!((writer.l0_intervals(), writer.l0_frames()), (2, 1));

    writer.set_max_index_bytes(FRAME + 2 * PAGE);
    for _ in 0..2 {
        let message = refused(write_byte_keys(&mut writer, &[long]).await.remove(0));
        assert!(message.contains("would grow the index past"), "{message}");
        assert_eq!((writer.keys_count(), writer.l0_intervals()), (5, 2));
    }
    writer.flush().await.unwrap();

    writer.set_max_index_bytes(FRAME + 2 * PAGE + FRAME);
    write_byte_keys(&mut writer, &[long])
        .await
        .remove(0)
        .unwrap();
    assert_eq!(writer.l0_intervals(), 2, "the key needed no page");
    writer.finish().await.unwrap();

    assert_eq!(
        file_bytes(&io, FileRole::Index).await.len() as u64,
        FRAME + 2 * PAGE + FRAME
    );

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.l0_frames(), 2);
    assert_eq!(reader.keys_count().await.unwrap(), 6);
    assert_eq!(
        reader
            .get(&Name(long.to_vec()), Hour(0), Hour(0))
            .await
            .unwrap(),
        Some(vec![Record::with(long.len() as u64)])
    );
}

#[tokio::test]
async fn bounds_that_grow_after_their_page_is_placed_count_toward_the_index_room() {
    // 34 bytes of room on a page: a three-byte key's entry is 9, an eleven-byte key's 17, and the two share a
    // page while a third key does not fit beside them. So every page starts with a three-byte key — an
    // interval of 12 bytes of L0 — and its upper bound then grows to the eleven-byte key, making it 20.
    const FRAME: usize = 300;
    const PAGE: usize = 46;
    const LIMIT: u64 = 12_200;

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), FRAME, PAGE)
        .await
        .unwrap();
    writer.set_max_index_bytes(LIMIT);

    let mut accepted: Vec<Vec<u8>> = Vec::new();

    'pages: for n in 0..1_000u64 {
        let short = format!("{n:03}").into_bytes();
        let long = format!("{n:03}zzzzzzzz").into_bytes();

        for key in [short, long] {
            match write_byte_keys(&mut writer, &[&key]).await.remove(0) {
                Ok(()) => accepted.push(key),
                Err(refusal) => {
                    let message = refused::<()>(Err(refusal));
                    assert!(message.contains("would grow the index past"), "{message}");
                    break 'pages;
                }
            }
        }
    }

    let written = (writer.l0_frames(), writer.l0_intervals(), accepted.len());

    // Counted as the 12 bytes each interval was when its page was placed, the L0 room comes out short: the
    // writes take 200 pages, and this flush runs into the limit — "300 more bytes at 12200 would grow the
    // index past 12200 bytes".
    writer
        .finish()
        .await
        .unwrap_or_else(|error| panic!("{written:?} written: {error}"));
    assert!(file_bytes(&io, FileRole::Index).await.len() as u64 <= LIMIT);

    // 173 pages, every one holding both keys, never flushed: all 173 intervals are 20 bytes when it runs,
    // and 14 of them fill a frame, so it needs 13.
    assert_eq!(written, (1, 173, 346));

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.l0_frames(), 13);
    assert_eq!(reader.keys_count().await.unwrap(), accepted.len());
    for key in &accepted {
        assert_eq!(
            reader
                .get(&Name(key.clone()), Hour(0), Hour(0))
                .await
                .unwrap(),
            Some(vec![Record::with(key.len() as u64)]),
            "{key:?}"
        );
    }
}

/// Writes `long` (when given) and then ascending eight-byte keys into 1 KiB pages and 4 KiB L0 frames, under
/// an index limit of frame 0 and 100 pages, until the first refusal: the intervals then, and the writer.
async fn intervals_at_the_first_refusal(long: Option<&[u8]>) -> (usize, InMemoryIo, Writer) {
    const FRAME: usize = 4096;
    const PAGE: usize = 1024;

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), FRAME, PAGE)
        .await
        .unwrap();
    writer.set_max_index_bytes((FRAME + 100 * PAGE) as u64);

    if let Some(long) = long {
        write_byte_keys(&mut writer, &[long])
            .await
            .remove(0)
            .unwrap();
    }

    for n in 0.. {
        let key = format!("{n:08}").into_bytes();

        if let Err(refusal) = write_byte_keys(&mut writer, &[&key]).await.remove(0) {
            let message = refused::<()>(Err(refusal));
            assert!(message.contains("would grow the index past"), "{message}");
            break;
        }
    }

    (writer.l0_intervals(), io, writer)
}

#[tokio::test]
async fn one_long_key_does_not_halve_the_index_a_writer_may_use() {
    // Eight-byte keys: 72 to a page, and 185 intervals to an L0 frame. A hundred pages need no continuation.
    let (short, _, _) = intervals_at_the_first_refusal(None).await;
    assert_eq!(short, 100);

    // The largest key a 1 KiB page accepts, first: 1,006 bytes of L0 entry at most, four to a frame. Counted
    // as long as that, every interval would reserve a quarter of a frame — as much as its page — and the
    // index would refuse at about 52 pages.
    let long = vec![b'!'; 499];
    let (with_long, io, writer) = intervals_at_the_first_refusal(Some(&long)).await;
    assert!(
        with_long >= short - 5,
        "{with_long} intervals with one long key, {short} without"
    );

    let keys = writer.keys_count();
    writer.finish().await.unwrap();
    assert!(file_bytes(&io, FileRole::Index).await.len() <= 4096 + 100 * 1024);

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), keys);
    assert_eq!(
        reader
            .get(&Name(long.clone()), Hour(0), Hour(0))
            .await
            .unwrap(),
        Some(vec![Record::with(long.len() as u64)])
    );
}

#[tokio::test]
async fn no_flush_runs_out_of_index_room_whatever_the_lengths_and_order_of_the_keys() {
    // Small pages and L0 frames near the least their pages allow, keys of every length a page accepts, an
    // index limit a few pages past frame 0, and a flush every few keys: a flush that needs more room than
    // the writes before it checked for is an `Err`. Seed 203 is one that fails when the check leaves out
    // the L0 bytes the key being placed adds.
    for seed in 0..1_000u64 {
        let mut rng = seeded(seed);
        let page = 40 + (rng.next() % 60) as usize;
        let longest = ((page - 12) / 2 - 5 - 1) as u64;
        let frame = 20 + 2 * (longest as usize + 1) + 4 + (rng.next() % 60) as usize;

        let count = 20 + rng.next() % 200;
        let mut keys: Vec<Vec<u8>> = (0..count)
            .map(|_| {
                let len = 1 + rng.next() % longest;
                (0..len).map(|_| 1 + (rng.next() % 250) as u8).collect()
            })
            .collect::<std::collections::BTreeSet<Vec<u8>>>()
            .into_iter()
            .collect();
        if seed % 3 != 0 {
            rng.shuffle(&mut keys);
        }

        let case = format!("seed {seed}: pages of {page} bytes, L0 frames of {frame}");
        let limit = (frame + (2 + rng.next() % 30) as usize * page) as u64;
        let flush_every = 1 + rng.next() % 20;

        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), frame, page)
            .await
            .unwrap();
        writer.set_max_index_bytes(limit);

        let mut accepted = Vec::new();

        for (n, key) in keys.iter().enumerate() {
            match write_byte_keys(&mut writer, &[key]).await.remove(0) {
                Ok(()) => accepted.push(key.clone()),
                Err(refusal) => {
                    refused::<()>(Err(refusal));
                }
            }

            if (n as u64).is_multiple_of(flush_every) {
                writer
                    .flush()
                    .await
                    .unwrap_or_else(|error| panic!("{case}, key {n}: {error}"));
            }
        }

        writer
            .finish()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        assert!(file_bytes(&io, FileRole::Index).await.len() as u64 <= limit);

        let reader = Reader::open(io).await.unwrap();
        assert_eq!(reader.keys_count().await.unwrap(), accepted.len(), "{case}");
        for key in &accepted {
            assert_eq!(
                reader
                    .get(&Name(key.clone()), Hour(0), Hour(0))
                    .await
                    .unwrap(),
                Some(vec![Record::with(key.len() as u64)]),
                "{case}: {key:?}"
            );
        }
    }
}
