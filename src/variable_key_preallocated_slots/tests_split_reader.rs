use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use super::split_test_support::{
    DATA_PAGE, DEFAULT_PAGES_PER_FILE, HB_A, HB_BRAVO, HB_C, HB_DATA_LEN, HB_INDEX_LEN, HB_K1,
    HB_K2_F1, HB_K2_F2, HB_K2_F3, HB_K10, HB_L0_0, HB_L0_1, HB_L1_A, HB_L1_A2, HB_L1_B, Reader,
    Row, Writer, assert_reads_back, build, compact, data_files, file_bytes, frame_offsets,
    hand_built, hb_kilo, hour_0_rows, key, le_u32_at, numbered_keys, parse_index, patch_u16,
    random_keys, reference_split_compact, rows, seeded, wait_until,
};
use super::split_test_support::{assert_still_waiting, poll_until_parked};
use super::test_types::{Hour, Name, Record};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::{
    ArchiveError, VariableKeyPreallocatedSlotsArchiveReaderSingleFile,
    VariableKeyPreallocatedSlotsArchiveWriterSingleFile, patch_l0_u32, patch_u32, restamp_l0,
};

type SingleFileWriter =
    VariableKeyPreallocatedSlotsArchiveWriterSingleFile<Name, Hour, Record, Cursor<Vec<u8>>>;
type SingleFileReader =
    VariableKeyPreallocatedSlotsArchiveReaderSingleFile<Name, Hour, Record, Cursor<Vec<u8>>>;

/// A finished archive of `keys` in slot 0, three keys to a 64-byte L1 page.
async fn archive_of(keys: &[Vec<u8>]) -> (InMemoryIo, Vec<Row>) {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 1024, 64)
        .await
        .unwrap();
    let rows = hour_0_rows(keys);

    for (key, hour, record) in &rows {
        writer
            .write(Name(key.clone()), Hour(*hour), record)
            .await
            .unwrap();
    }

    writer.finish().await.unwrap();
    (io, rows)
}

#[tokio::test]
async fn get_is_inclusive_empty_when_from_is_after_to_and_none_for_an_absent_key() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    for hour in [3, 4, 6] {
        writer
            .write(
                Name(b"mid".to_vec()),
                Hour(hour),
                &Record::with(hour as u64),
            )
            .await
            .unwrap();
    }
    writer
        .write(Name(b"top".to_vec()), Hour(0), &Record::with(9))
        .await
        .unwrap();
    writer.finish().await.unwrap();

    let reader = Reader::open(io.clone()).await.unwrap();
    let index = io.file(FileRole::Index).unwrap();
    let data_0 = io.file(FileRole::Data(0)).unwrap();
    let opened = index.read_ats();

    assert_eq!(
        reader
            .get(&Name(b"mid".to_vec()), Hour(5), Hour(4))
            .await
            .unwrap(),
        Some(vec![])
    );
    assert_eq!(
        reader
            .get(&Name(b"zzz".to_vec()), Hour(5), Hour(4))
            .await
            .unwrap(),
        Some(vec![])
    );
    assert!(
        reader.l1_cache.pages.read().await.is_empty(),
        "nothing was read"
    );
    assert_eq!(index.read_ats(), opened);
    assert_eq!((data_0.readers_opened(), data_0.read_ats()), (0, 0));

    for absent in [&b"a"[..], b"mi", b"mid\0", b"tops", b"zzz"] {
        assert_eq!(
            reader
                .get(&Name(absent.to_vec()), Hour(0), Hour(23))
                .await
                .unwrap(),
            None
        );
    }
    assert_eq!(
        data_0.readers_opened(),
        0,
        "an absent key reads no data file"
    );

    assert_eq!(
        reader
            .get(&Name(b"mid".to_vec()), Hour(3), Hour(6))
            .await
            .unwrap(),
        Some(vec![
            Record::with(3),
            Record::with(4),
            Record::default(),
            Record::with(6)
        ])
    );
    assert_eq!(
        reader
            .get(&Name(b"mid".to_vec()), Hour(4), Hour(4))
            .await
            .unwrap(),
        Some(vec![Record::with(4)])
    );
}

#[tokio::test]
async fn an_empty_archive_answers_none_without_opening_a_data_file() {
    let io = InMemoryIo::default();
    Writer::new(io.clone())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();

    let reader = Reader::open(io.clone()).await.unwrap();

    assert_eq!(reader.l0_intervals(), 0);
    assert_eq!(reader.keys_count().await.unwrap(), 0);
    assert_eq!(
        reader
            .get(&Name(b"x".to_vec()), Hour(0), Hour(0))
            .await
            .unwrap(),
        None
    );
    assert_eq!(io.roles(), [FileRole::Index]);
}

#[tokio::test]
async fn a_truncated_or_garbage_archive_is_an_error_not_an_absent_key() {
    let garbage = InMemoryIo::default();
    garbage.put(FileRole::Index, vec![1, 2, 3]);
    assert!(matches!(
        Reader::open(garbage).await,
        Err(ArchiveError::Corrupted(_))
    ));

    let (io, _) = archive_of(&[b"five".to_vec()]).await;
    let data = file_bytes(&io, FileRole::Data(0)).await;
    let five = Name(b"five".to_vec());

    let cut = InMemoryIo::default();
    cut.put(FileRole::Index, file_bytes(&io, FileRole::Index).await);
    cut.put(FileRole::Data(0), data[..data.len() - 100].to_vec());
    let reader = Reader::open(cut.clone()).await.unwrap();
    assert!(matches!(
        reader.get(&five, Hour(0), Hour(0)).await,
        Err(ArchiveError::Corrupted(_))
    ));
    assert_eq!(cut.file(FileRole::Data(0)).unwrap().eof_reads(), 0);

    let reader = Reader::open(io.clone()).await.unwrap();
    io.file(FileRole::Data(0)).unwrap().fail_reads();
    assert!(matches!(
        reader.get(&five, Hour(0), Hour(0)).await,
        Err(ArchiveError::Io(_))
    ));
}

#[tokio::test]
async fn the_reader_caches_are_bounded_by_page_count() {
    let mut rng = seeded(10);
    let (rows, names) = rows(&numbered_keys(40), &mut rng);
    let reader = Reader::open(build(&rows, 1024, 64, 2, None).await)
        .await
        .unwrap();

    reader.set_l1_cache_size(1).await;
    reader.set_data_cache_size(2).await;
    assert_reads_back(&reader, &names).await;

    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);
    assert_eq!(reader.data_cache.pages.read().await.len(), 2);

    let last = names.keys().last().unwrap().clone();
    let at = reader
        .data_ref_of(&Name(last))
        .await
        .unwrap()
        .unwrap()
        .cache_key();
    assert!(
        reader.data_cache.pages.read().await.contains_key(&at),
        "the data cache is keyed by the page's data_ref"
    );

    reader.set_l1_cache_size(0).await;
    reader.set_data_cache_size(0).await;
    assert_reads_back(&reader, &names).await;
}

#[tokio::test]
async fn a_miss_reads_without_holding_a_cache_lock_or_blocking_the_same_file() {
    let (io, _) = archive_of(&numbered_keys(4)).await;
    let data_0 = io.file(FileRole::Data(0)).unwrap();
    let reader = Arc::new(Reader::open(io).await.unwrap());

    let value = |n: u64| Record::with(key(n).iter().map(|byte| *byte as u64).sum());

    reader.get(&Name(key(1)), Hour(0), Hour(0)).await.unwrap();

    let parked_at = reader.data_ref_of(&Name(key(3))).await.unwrap().unwrap();
    data_0.shut_reads_at(parked_at.offset as u64);

    let missing = tokio::spawn({
        let reader = reader.clone();
        async move { reader.get(&Name(key(3)), Hour(0), Hour(0)).await.unwrap() }
    });

    wait_until("the miss parking inside its read", || {
        data_0.is_read_waiting()
    })
    .await;

    let hit = tokio::time::timeout(
        Duration::from_secs(2),
        reader.get(&Name(key(1)), Hour(0), Hour(0)),
    )
    .await
    .expect("a cached read waited on a read in flight");
    assert_eq!(hit.unwrap().unwrap(), vec![value(1)]);

    tokio::time::timeout(Duration::from_secs(2), async {
        drop(reader.l1_cache.pages.write().await);
        drop(reader.data_cache.pages.write().await);
    })
    .await
    .expect("a cache lock was held across a read");

    let other = tokio::time::timeout(
        Duration::from_secs(2),
        reader.get(&Name(key(2)), Hour(0), Hour(0)),
    )
    .await
    .expect("a miss of another page of the same data file waited on the parked read");
    assert_eq!(other.unwrap().unwrap(), vec![value(2)]);
    assert!(data_0.is_read_waiting());

    data_0.open_reads();
    assert_eq!(missing.await.unwrap().unwrap(), vec![value(3)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn data_readers_are_opened_lazily_and_once_per_file() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(2 * DATA_PAGE as u64);
    for n in 1..=6u64 {
        writer
            .write(Name(key(n)), Hour(0), &Record::with(n))
            .await
            .unwrap();
    }
    writer.finish().await.unwrap();

    let opened = |io: &InMemoryIo| -> Vec<usize> {
        (0..3)
            .map(|no| io.file(FileRole::Data(no)).unwrap().readers_opened())
            .collect()
    };

    let reader = Arc::new(Reader::open(io.clone()).await.unwrap());
    reader.set_data_cache_size(0).await;
    assert_eq!(opened(&io), [0, 0, 0]);
    assert_eq!(io.file(FileRole::Index).unwrap().readers_opened(), 1);

    assert_eq!(
        reader
            .data_ref_of(&Name(key(3)))
            .await
            .unwrap()
            .unwrap()
            .file_no,
        1
    );
    assert_eq!(
        reader.get(&Name(key(3)), Hour(0), Hour(0)).await.unwrap(),
        Some(vec![Record::with(3)])
    );
    assert_eq!(opened(&io), [0, 1, 0]);

    // Concurrent FIRST uses of data file 2, and concurrent later uses of data file 1.
    let tasks: Vec<_> = (0..16u64)
        .map(|task| {
            let reader = reader.clone();
            let n = 3 + task % 4;
            tokio::spawn(async move {
                reader.get(&Name(key(n)), Hour(0), Hour(0)).await.unwrap()
                    == Some(vec![Record::with(n)])
            })
        })
        .collect();

    for task in tasks {
        assert!(task.await.unwrap());
    }
    assert_eq!(opened(&io), [0, 1, 1]);
}

#[tokio::test]
async fn a_usual_page_costs_one_read_at_and_no_read_passes_the_end() {
    let mut rng = seeded(8);
    let keys = numbered_keys(1_100);
    let (rows, names) = rows(&keys, &mut rng);

    let source = Reader::open(build(&rows, 64 * 1024, 16 * 1024, 64, None).await)
        .await
        .unwrap();
    let io = compact(&source).await;

    let index = io.file(FileRole::Index).unwrap();
    let data_0 = io.file(FileRole::Data(0)).unwrap();
    assert!(
        index.bytes().await.len() < 64 * 1024,
        "the test needs a short index"
    );

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(
        index.read_ats(),
        2,
        "open reads the whole L0 in one read, and the header of the last L1 page in one more"
    );
    assert_eq!(reader.l0_intervals(), 2);

    // The first key's page, then the last key's: the last L1 page is shorter than the 16 KiB guess and
    // ends exactly where the index does.
    for (n, key) in [&keys[0], &keys[keys.len() - 1]].into_iter().enumerate() {
        reader
            .get(&Name(key.clone()), Hour(0), Hour(23))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(index.read_ats(), 3 + n, "one read per L1 page");
        assert_eq!(data_0.read_ats(), 1 + n, "one read per data page");
    }

    assert_reads_back(&reader, &names).await;
    assert_eq!((index.eof_reads(), data_0.eof_reads()), (0, 0));
}

#[tokio::test]
async fn continuation_frames_are_followed_on_l0_l1_and_data() {
    let archive = hand_built();
    let io = archive.io();
    let reader = Reader::open(io.clone()).await.unwrap();

    assert_eq!(reader.l0_frames(), 2);
    assert_eq!(reader.l0_intervals(), 2);
    assert_eq!(reader.keys_count().await.unwrap(), 4);
    assert_reads_back(&reader, &archive.names).await;

    // Slot 7 of `bravo` straddles the first two frames of its data page; `b` is 98, so slot 8 is written.
    assert_eq!(
        reader
            .get(&Name(HB_BRAVO.to_vec()), Hour(7), Hour(9))
            .await
            .unwrap(),
        Some(vec![
            Record::default(),
            Record::with(9808),
            Record::default()
        ])
    );
    assert_eq!(
        reader
            .get(&Name(b"d".to_vec()), Hour(0), Hour(0))
            .await
            .unwrap(),
        None
    );

    let out = compact(&reader).await;
    let (index, data) = reference_split_compact(&archive.names, DEFAULT_PAGES_PER_FILE);
    assert!(file_bytes(&out, FileRole::Index).await == index);
    assert!(data_files(&out).await == data);

    for role in io.roles() {
        assert_eq!(io.file(role).unwrap().eof_reads(), 0, "{role:?}");
    }
}

/// Which call first noticed the damage — `open`, a `get` of every key, `keys_count`, `compaction_plan` —
/// with every other outcome than `Corrupted` or success failing the test.
async fn corruption_seen(io: InMemoryIo) -> Option<&'static str> {
    let reader = match Reader::open(io).await {
        Ok(reader) => reader,
        Err(ArchiveError::Corrupted(_)) => return Some("open"),
        Err(other) => panic!("open: {other}"),
    };

    let mut seen = None;

    for key in [HB_A.to_vec(), HB_BRAVO.to_vec(), HB_C.to_vec(), hb_kilo()] {
        match reader.get(&Name(key.clone()), Hour(0), Hour(23)).await {
            Ok(_) => {}
            Err(ArchiveError::Corrupted(_)) => seen = seen.or(Some("get")),
            Err(other) => panic!("get {key:?}: {other}"),
        }
    }

    match reader.keys_count().await {
        Ok(_) => {}
        Err(ArchiveError::Corrupted(_)) => seen = seen.or(Some("keys_count")),
        Err(other) => panic!("keys_count: {other}"),
    }

    match reader.compaction_plan().await {
        Ok(_) => {}
        Err(ArchiveError::Corrupted(_)) => seen = seen.or(Some("compaction_plan")),
        Err(other) => panic!("compaction_plan: {other}"),
    }

    seen
}

type Mutation = (&'static str, FileRole, fn(&mut Vec<u8>));

#[tokio::test]
async fn corrupted_headers_chains_and_refs_are_errors_never_none() {
    let archive = hand_built();
    assert_eq!(corruption_seen(archive.io()).await, None);

    let index = FileRole::Index;
    let data = FileRole::Data(0);

    // The long key's prefix is two bytes, so its key starts 2 past its prefix and is 130 bytes long.
    let cases: [Mutation; 32] = [
        ("L0: frame 0 chains into itself", index, |b| {
            patch_l0_u32(b, HB_L0_0, 4, 4)
        }),
        ("L0: a continuation chains into frame 0", index, |b| {
            patch_l0_u32(b, HB_L0_1, 4, 8)
        }),
        ("L0: a continuation chains to itself", index, |b| {
            patch_l0_u32(b, HB_L0_1, 4, HB_L0_1 as u32)
        }),
        ("L0: a continuation chains backwards", index, |b| {
            patch_l0_u32(b, HB_L0_1, 4, HB_L1_A as u32)
        }),
        ("L0: a next past the end", index, |b| {
            patch_l0_u32(b, HB_L0_0, 4, HB_INDEX_LEN as u32 + 100)
        }),
        ("L0: a size shorter than the header", index, |b| {
            patch_l0_u32(b, HB_L0_0, 0, 8)
        }),
        ("L0: a size past the end", index, |b| {
            patch_l0_u32(b, HB_L0_1, 0, 4096)
        }),
        ("L0: a count above the intervals", index, |b| {
            patch_l0_u32(b, HB_L0_0, 8, 2)
        }),
        ("L0: a count below the intervals", index, |b| {
            patch_l0_u32(b, HB_L0_1, 8, 0)
        }),
        ("L0: a count patched without its hash", index, |b| {
            patch_u32(b, HB_L0_0 + 8, 0)
        }),
        ("L0: intervals out of order across frames", index, |b| {
            b[HB_L0_1 + 20 + 2] = b'b'
        }),
        ("L0: an empty key inside an interval", index, |b| {
            b[HB_L0_0 + 20 + 2] = 0
        }),
        ("L0: an index of five bytes", index, |b| b.truncate(5)),
        ("L1: a next backwards", index, |b| {
            patch_u32(b, HB_L1_A + 4, 20)
        }),
        ("L1: a next past the end", index, |b| {
            patch_u32(b, HB_L1_A + 4, HB_INDEX_LEN as u32 + 1)
        }),
        ("L1: a count of 0 on a continuation", index, |b| {
            patch_u32(b, HB_L1_A2 + 8, 0)
        }),
        ("L1: more entries than the body holds", index, |b| {
            patch_u32(b, HB_L1_B + 8, 100)
        }),
        ("L1: an entry cut at the end of its frame", index, |b| {
            patch_u32(b, HB_L1_A, 29)
        }),
        ("L1: a short length spelled in two bytes", index, |b| {
            b[HB_L1_B + 12 + 1] = 5
        }),
        ("L1: keys not ascending across frames", index, |b| {
            b[HB_L1_A2 + 12 + 1] = b'a'
        }),
        (
            "L1: a first key other than the interval's from",
            index,
            |b| b[HB_L0_0 + 20 + 1] = b'0',
        ),
        ("L1: a last key other than the interval's to", index, |b| {
            b[HB_L0_0 + 20 + 3] = b'd'
        }),
        ("L1: an l1_offset at the end of the index", index, |b| {
            patch_u32(b, HB_L0_1 + 20 + 132 + 132, HB_INDEX_LEN as u32)
        }),
        ("data: a ref offset past the end of its file", index, |b| {
            patch_u32(b, HB_L1_B + 12 + 132 + 1, HB_DATA_LEN as u32 + 10)
        }),
        ("data: a next backwards", data, |b| {
            patch_u32(b, HB_K2_F2 + 4, HB_K2_F1 as u32)
        }),
        ("data: a next past the end", data, |b| {
            patch_u32(b, HB_K2_F1 + 4, HB_DATA_LEN as u32 + 5)
        }),
        ("data: a wrong item_len on frame 0", data, |b| {
            patch_u16(b, HB_K1 + 8, 127)
        }),
        ("data: a wrong item_len on a continuation", data, |b| {
            patch_u16(b, HB_K2_F2 + 8, 127)
        }),
        ("data: bodies short of SLOTS × ITEM_LEN", data, |b| {
            patch_u32(b, HB_K2_F3, 1582 - 128)
        }),
        ("data: a body past SLOTS × ITEM_LEN", data, |b| {
            patch_u32(b, HB_K1, 3092)
        }),
        ("data: a size shorter than the header", data, |b| {
            patch_u32(b, HB_K10, 9)
        }),
        ("data: a size past the end", data, |b| {
            patch_u32(b, HB_K10, 5000)
        }),
    ];

    for (case, role, mutate) in cases {
        let io = archive.io();
        let mut bytes = file_bytes(&io, role).await;
        mutate(&mut bytes);
        let file = io.put(role, bytes);

        assert!(corruption_seen(io).await.is_some(), "{case} was accepted");
        assert_eq!(file.eof_reads(), 0, "{case}: a read passed the end");
    }

    let io = archive.io();
    let mut bytes = archive.index.clone();
    bytes[HB_L1_B + 12 + 132] = 7;
    io.put(FileRole::Index, bytes);

    let reader = Reader::open(io).await.unwrap();
    assert!(
        matches!(
            reader.get(&Name(hb_kilo()), Hour(0), Hour(0)).await,
            Err(ArchiveError::Io(_))
        ),
        "a ref into a data file that does not exist is the factory's error"
    );
}

#[tokio::test]
async fn an_l1_frame_whose_count_disagrees_with_its_entries_is_corrupted() {
    let archive = hand_built();

    // Frame 0 of the page of `a`..`c` holds 2 entries, its continuation 1; both frames are padded, so a
    // count above the entries still passes the header check and only the walk can notice.
    for (frame, count) in [(HB_L1_A, 3), (HB_L1_A, 1), (HB_L1_A2, 2), (HB_L1_A2, 0)] {
        let case = format!("count {count} on the frame at {frame}");
        let mut bytes = archive.index.clone();
        patch_u32(&mut bytes, frame + 8, count);

        let io = archive.io();
        io.put(FileRole::Index, bytes);
        let reader = Reader::open(io).await.unwrap();

        for key in [HB_A, HB_C] {
            assert!(
                matches!(
                    reader.get(&Name(key.to_vec()), Hour(0), Hour(0)).await,
                    Err(ArchiveError::Corrupted(_))
                ),
                "{case}: get {key:?}"
            );
        }
        assert!(
            matches!(
                reader.compaction_plan().await,
                Err(ArchiveError::Corrupted(_))
            ),
            "{case}: compaction_plan"
        );
    }
}

#[tokio::test]
async fn a_corrupted_l0_chain_is_refused() {
    let mut rng = seeded(5150);
    let (rows, _) = rows(&numbered_keys(120), &mut rng);
    let io = build(&rows, 68, 64, 2, None).await;
    let bytes = file_bytes(&io, FileRole::Index).await;

    let offsets = frame_offsets(&bytes);
    assert!(offsets.len() >= 3, "the test needs three frames");

    let with_next = |frame: usize, next: u32| {
        let mut bytes = bytes.clone();
        patch_l0_u32(&mut bytes, offsets[frame], 4, next);
        bytes
    };

    let count = le_u32_at(&bytes, 8);
    let with_count = |count: u32| {
        let mut bytes = bytes.clone();
        patch_l0_u32(&mut bytes, 0, 8, count);
        bytes
    };

    // The first `from` of frame 1 below everything in frame 0: `a` sorts before `k`.
    let mut unordered = bytes.clone();
    unordered[offsets[1] + 20 + 1] = b'a';

    let cases = [
        ("a next into frame 0", with_next(0, 8)),
        ("a next to itself", with_next(1, offsets[1] as u32)),
        (
            "a next back to an earlier frame",
            with_next(2, offsets[1] as u32),
        ),
        (
            "a next into frame 0 from a continuation",
            with_next(2, 0x10),
        ),
        (
            "a next past the end",
            with_next(1, bytes.len() as u32 + 4096),
        ),
        ("a count above the entries", with_count(count + 1)),
        ("a count below the entries", with_count(count - 1)),
        ("intervals unordered across frames", unordered),
    ];

    for (case, bytes) in cases {
        let io = InMemoryIo::default();
        io.put(FileRole::Index, bytes);

        assert!(
            matches!(Reader::open(io).await, Err(ArchiveError::Corrupted(_))),
            "{case} was accepted"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_reader_and_compaction_run_inside_spawned_tasks() {
    let mut rng = seeded(314);
    let keys = random_keys(1_500, 200, &mut rng);
    let (rows, names) = rows(&keys, &mut rng);
    let io = InMemoryIo::default();

    tokio::spawn({
        let io = io.clone();
        async move {
            let mut writer = Writer::new_with_capacity(io, 4096, 2048).await?;
            writer.set_l1_cache_size(4);
            writer.set_data_cache_size(100);
            writer.set_max_data_file_bytes(400 * DATA_PAGE as u64);

            for (key, hour, record) in &rows {
                writer.write(Name(key.clone()), Hour(*hour), record).await?;
            }

            writer.finish().await
        }
    })
    .await
    .unwrap()
    .unwrap();

    let reader = Arc::new(Reader::open(io).await.unwrap());
    let names = Arc::new(names);

    let readers: Vec<_> = (0..4)
        .map(|part| {
            let reader = reader.clone();
            let names = names.clone();
            tokio::spawn(async move {
                for (key, hours) in names.iter().skip(part).step_by(4) {
                    let items = reader.get(&Name(key.clone()), Hour(0), Hour(23)).await?;
                    let expected: Vec<Record> = hours
                        .iter()
                        .map(|hour| hour.clone().unwrap_or_default())
                        .collect();
                    assert!(items == Some(expected), "key {key:?}");
                }
                Ok::<_, ArchiveError>(())
            })
        })
        .collect();

    for task in readers {
        task.await.unwrap().unwrap();
    }

    let out = InMemoryIo::default();
    tokio::spawn({
        let reader = reader.clone();
        let out = out.clone();
        async move {
            let plan = reader.compaction_plan().await?;
            reader.write_compacted_to(&plan, &out).await
        }
    })
    .await
    .unwrap()
    .unwrap();

    let (index, data) = reference_split_compact(&names, DEFAULT_PAGES_PER_FILE);
    assert!(file_bytes(&out, FileRole::Index).await == index);
    assert!(data_files(&out).await == data);
}

/// Why `open` refused `index`, which it must have done as `Corrupted` after reading only the first frame.
async fn open_error(index: Vec<u8>) -> String {
    let io = InMemoryIo::default();
    let file = io.put(FileRole::Index, index);

    let message = match Reader::open(io).await {
        Err(ArchiveError::Corrupted(message)) => message,
        Err(other) => panic!("not Corrupted: {other}"),
        Ok(reader) => panic!("opened, with {} intervals", reader.l0_intervals()),
    };

    assert_eq!(file.read_ats(), 1, "{message}");
    message
}

#[tokio::test]
async fn a_single_file_object_or_random_bytes_put_as_the_index_are_refused_as_corrupted() {
    let mut rng = seeded(2718);

    for count in [0, 3, 3_000] {
        let keys = random_keys(count, 40, &mut rng);
        let (rows, _) = rows(&keys, &mut rng);

        let mut writer = SingleFileWriter::new(Cursor::new(Vec::new()));
        for (key, hour, record) in &rows {
            writer
                .write(Name(key.clone()), Hour(*hour), record)
                .await
                .unwrap();
        }
        let working = writer.finish().await.unwrap().into_inner();

        let mut compact = Vec::new();
        SingleFileReader::open(Cursor::new(working.clone()))
            .await
            .unwrap()
            .write_compacted_to(&mut compact)
            .await
            .unwrap();

        for (shape, bytes) in [("working", working), ("compact", compact)] {
            let long_enough = bytes.len() >= 20;
            let message = open_error(bytes).await;

            if long_enough {
                assert!(
                    message.contains("not a split archive index"),
                    "{count} keys, {shape}: {message}"
                );
            }
        }
    }

    let random: Vec<u8> = (0..4096).map(|_| rng.next() as u8).collect();

    for bytes in [random, vec![0; 4096], vec![0xFF; 64]] {
        assert!(
            open_error(bytes)
                .await
                .contains("not a split archive index")
        );
    }
}

#[tokio::test]
async fn an_index_that_ends_before_an_l1_page_its_l0_names_is_refused_at_open() {
    let (io, _) = archive_of(&numbered_keys(9)).await;
    let index = file_bytes(&io, FileRole::Index).await;
    let reader = Reader::open(io).await.unwrap();
    assert!(reader.l0_intervals() > 1, "the test needs several L1 pages");

    for len in [1024, 1024 + 11, index.len() - 64 + 11] {
        let message = open_error(index[..len].to_vec()).await;
        assert!(message.contains("past the end"), "{len} bytes: {message}");
    }
}

#[tokio::test]
async fn flipping_any_bit_of_the_header_of_frame_0_or_of_a_continuation_is_corrupted_at_open() {
    let mut rng = seeded(31);
    let keys = random_keys(40, 40, &mut rng);
    let (rows, _) = rows(&keys, &mut rng);

    // L0 frames of 130 bytes hold one or two intervals of keys up to 40 bytes: a chain of many frames.
    let index = file_bytes(&build(&rows, 130, 128, 2, None).await, FileRole::Index).await;
    let frames = frame_offsets(&index);
    assert!(frames.len() > 3, "the test must chain");

    for frame in [frames[0], frames[1], frames[frames.len() - 1]] {
        for bit in 0..20 * 8 {
            let mut bytes = index.clone();
            bytes[frame + bit / 8] ^= 1 << (bit % 8);

            let io = InMemoryIo::default();
            io.put(FileRole::Index, bytes);

            match Reader::open(io).await {
                Err(ArchiveError::Corrupted(message)) => assert!(
                    message.contains(&format!("L0 frame at {frame} fails its header hash")),
                    "frame {frame}, bit {bit}: {message}"
                ),
                Err(other) => panic!("frame {frame}, bit {bit}: {other}"),
                Ok(_) => panic!("frame {frame}, bit {bit}: opened"),
            }
        }
    }
}

/// `open` over `index` alone: the intervals it read, or why it refused as `Corrupted`; and the `read_at`s
/// it cost.
async fn open_index(index: Vec<u8>) -> (Result<usize, String>, usize) {
    let io = InMemoryIo::default();
    let file = io.put(FileRole::Index, index);

    let opened = match Reader::open(io).await {
        Ok(reader) => Ok(reader.l0_intervals()),
        Err(ArchiveError::Corrupted(message)) => Err(message),
        Err(other) => panic!("not Corrupted: {other}"),
    };

    (opened, file.read_ats())
}

/// The compact archive of `count` numbered keys, and the keys.
async fn compact_keys(count: u64, seed: u64) -> (InMemoryIo, Vec<Vec<u8>>) {
    let mut rng = seeded(seed);
    let keys = numbered_keys(count);
    let (rows, _) = rows(&keys, &mut rng);

    let source = Reader::open(build(&rows, 64 * 1024, 16 * 1024, 64, None).await)
        .await
        .unwrap();

    (compact(&source).await, keys)
}

#[tokio::test]
async fn an_index_cut_anywhere_inside_its_last_l1_page_is_refused_at_open() {
    // 2,500 nine-byte keys: three compact L1 pages, of 1,024, 1,024 and 452 keys of 15 bytes each, the last
    // ending where the index does.
    let (io, _) = compact_keys(2_500, 99).await;
    let index = file_bytes(&io, FileRole::Index).await;
    let parsed = parse_index(&index, None);
    assert_eq!(parsed.counts(), [1_024, 1_024, 452]);

    let last = parsed.intervals[2].2 as usize;
    assert_eq!(parsed.sizes[2], 12 + 452 * 15);
    assert_eq!(index.len(), last + parsed.sizes[2]);

    let (opened, read_ats) = open_index(index.clone()).await;
    assert_eq!(opened, Ok(3));
    assert_eq!(read_ats, 2, "L0, then the header of the last L1 page");

    // A cut at the page's start or inside its header needs no read past L0 to be seen; any cut after the
    // header — what an append that failed partway leaves behind — costs the one read of that header.
    for cut in [
        last,
        last + 11,
        last + 12,
        last + 12 + 3 * 15,
        last + 4_096,
        index.len() - 15,
        index.len() - 1,
    ] {
        let (opened, read_ats) = open_index(index[..cut].to_vec()).await;

        match opened {
            Err(message) => assert!(message.contains("past the end"), "cut at {cut}: {message}"),
            Ok(intervals) => panic!("cut at {cut}: opened with {intervals} intervals"),
        }
        assert_eq!(
            read_ats,
            if cut < last + 12 { 1 } else { 2 },
            "cut at {cut}"
        );
    }

    // A working index is refused alike: nine keys, three to a 64-byte page, the last page last in the index.
    let (working, _) = archive_of(&numbered_keys(9)).await;
    let working = file_bytes(&working, FileRole::Index).await;
    let last = working.len() - 64;
    assert_eq!(
        parse_index(&working, Some(64)).intervals[2].2 as usize,
        last
    );
    assert_eq!(open_index(working.clone()).await.0, Ok(3));

    for cut in [last + 12, last + 12 + 15, working.len() - 1] {
        let (opened, _) = open_index(working[..cut].to_vec()).await;
        assert!(opened.is_err(), "a working index cut at {cut}: {opened:?}");
    }
}

#[tokio::test]
async fn a_resize_landing_while_a_miss_waits_to_insert_leaves_neither_cache_above_it() {
    let keys = numbered_keys(40);
    let (io, rows) = archive_of(&keys).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let index = io.file(FileRole::Index).unwrap();
    let data_0 = io.file(FileRole::Data(0)).unwrap();

    // Both caches full at their default of 10.
    for (key, _, _) in &rows[..30] {
        reader
            .get(&Name(key.clone()), Hour(0), Hour(0))
            .await
            .unwrap();
    }

    // The L1 page of the last key is read with no lock held, and parks at its insert under the write lock
    // the test holds, while a resize to 1 lands.
    let (last, _, last_record) = &rows[39];
    let last = Name(last.clone());
    let page_of_last = reader
        .l0
        .iter()
        .find(|interval| interval.from <= last && last <= interval.to)
        .unwrap()
        .l1_offset;
    index.shut_reads_at(page_of_last as u64);
    {
        let call = reader.get(&last, Hour(0), Hour(0));
        tokio::pin!(call);
        poll_until_parked(call.as_mut(), || index.is_read_waiting()).await;

        let mut pages = reader.l1_cache.pages.write().await;
        index.open_reads();
        assert_still_waiting(call.as_mut()).await;
        reader.l1_cache.resize_locked(&mut pages, 1);
        drop(pages);

        assert_eq!(call.await.unwrap(), Some(vec![last_record.clone()]));
    }
    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);

    // The same for the data page of the key before it, its L1 page read into the cache first.
    let (before, _, before_record) = &rows[38];
    let before = Name(before.clone());
    let data_of_before = reader.data_ref_of(&before).await.unwrap().unwrap();
    data_0.shut_reads_at(data_of_before.offset as u64);
    {
        let call = reader.get(&before, Hour(0), Hour(0));
        tokio::pin!(call);
        poll_until_parked(call.as_mut(), || data_0.is_read_waiting()).await;

        let mut pages = reader.data_cache.pages.write().await;
        data_0.open_reads();
        assert_still_waiting(call.as_mut()).await;
        reader.data_cache.resize_locked(&mut pages, 1);
        drop(pages);

        assert_eq!(call.await.unwrap(), Some(vec![before_record.clone()]));
    }
    assert_eq!(reader.data_cache.pages.read().await.len(), 1);
    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);
}

#[tokio::test]
async fn a_cache_size_set_on_the_reader_evicts_at_once_and_the_caches_stay_within_it() {
    let keys = numbered_keys(40);
    let (io, rows) = archive_of(&keys).await;
    let reader = Reader::open(io).await.unwrap();

    let lens = || async {
        (
            reader.l1_cache.pages.read().await.len(),
            reader.data_cache.pages.read().await.len(),
        )
    };

    let read_all = || async {
        for (key, _, record) in &rows {
            assert_eq!(
                reader
                    .get(&Name(key.clone()), Hour(0), Hour(0))
                    .await
                    .unwrap(),
                Some(vec![record.clone()]),
                "key {key:?}"
            );
        }
    };

    // Fourteen L1 pages and forty data pages read: both caches full at their default of 10.
    read_all().await;
    assert_eq!(lens().await, (10, 10));

    reader.set_l1_cache_size(0).await;
    reader.set_data_cache_size(0).await;
    assert_eq!(lens().await, (0, 0), "0 empties the caches at once");

    read_all().await;
    assert_eq!(lens().await, (0, 0), "and they stay empty");

    reader.set_l1_cache_size(3).await;
    reader.set_data_cache_size(3).await;
    read_all().await;
    assert_eq!(lens().await, (3, 3));

    reader.set_l1_cache_size(1).await;
    assert_eq!(lens().await, (1, 3), "each cache on its own");
}

#[tokio::test]
async fn two_l0_intervals_naming_one_l1_page_are_an_error_warm_or_cold_never_a_wrong_answer() {
    let (io, keys) = compact_keys(2_500, 17).await;
    let mut index = file_bytes(&io, FileRole::Index).await;

    // L0 entry 1 names page 0 instead of page 1. Every bound is nine bytes, so an entry is
    // `1 + 9 + 1 + 9 + 4`. The hash covers only the header; restamped all the same.
    let l1_offset_of = |n: usize| 20 + n * 24 + 20;
    let page_0 = le_u32_at(&index, l1_offset_of(0));
    assert_eq!(le_u32_at(&index, l1_offset_of(1)), page_0 + 12 + 1_024 * 15);
    patch_u32(&mut index, l1_offset_of(1), page_0);
    restamp_l0(&mut index, 0);

    let data = file_bytes(&io, FileRole::Data(0)).await;

    for warm in [false, true] {
        let damaged = InMemoryIo::default();
        damaged.put(FileRole::Index, index.clone());
        damaged.put(FileRole::Data(0), data.clone());
        let reader = Reader::open(damaged).await.unwrap();

        if warm {
            // Page 0 checked against its own interval, and cached.
            reader
                .get(&Name(keys[0].clone()), Hour(0), Hour(0))
                .await
                .unwrap()
                .unwrap();
        }

        let corrupted = |result: Result<(), ArchiveError>, case: &str| match result {
            Err(ArchiveError::Corrupted(message)) => {
                assert!(message.contains("does not span"), "{case}: {message}")
            }
            other => panic!("{case}: {other:?}"),
        };

        for at in [1_024, 1_500, 2_047] {
            corrupted(
                reader
                    .get(&Name(keys[at].clone()), Hour(0), Hour(23))
                    .await
                    .map(|_| ()),
                &format!("warm {warm}: get of key {at}"),
            );
        }

        for (from, to) in [
            (Vec::new(), vec![0xFF; 16]),
            (keys[1_000].clone(), keys[1_030].clone()),
        ] {
            corrupted(
                reader
                    .get_range(&Name(from.clone()), &Name(to.clone()), Hour(0), Hour(23))
                    .await
                    .map(|_| ()),
                &format!("warm {warm}: range {from:?}..={to:?}"),
            );
        }

        // What the damage does not reach still reads.
        for at in [0, 1_023, 2_048, 2_499] {
            assert_eq!(
                reader
                    .get(&Name(keys[at].clone()), Hour(0), Hour(0))
                    .await
                    .unwrap()
                    .map(|items| items.len()),
                Some(1),
                "warm {warm}: key {at}"
            );
        }
    }
}
