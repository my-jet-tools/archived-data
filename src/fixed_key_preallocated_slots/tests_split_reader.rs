use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use super::split_test_support::{
    DATA_PAGE, DEFAULT_PAGES_PER_FILE, HB_DATA_LEN, HB_INDEX_LEN, HB_K1, HB_K2_F1, HB_K2_F2,
    HB_K2_F3, HB_K10, HB_L0_0, HB_L0_1, HB_L1_A, HB_L1_A2, HB_L1_B, Reader, Writer,
    assert_reads_back, build, compact, data_files, file_bytes, frame_offsets, hand_built,
    l0_frame_len, l1_page_len, logins, patch_u16, patch_u64, reference_split_day, rows, wait_until,
};
use super::split_test_support::{assert_still_waiting, poll_until_parked};
use super::test_types::{Hour, Login, Record, Rng};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::{
    ArchiveError, FixedKeyPreallocatedSlotsArchiveReaderSingleFile,
    FixedKeyPreallocatedSlotsArchiveWriterSingleFile, patch_l0_u32, patch_u32, restamp_l0,
};

type SingleFileWriter =
    FixedKeyPreallocatedSlotsArchiveWriterSingleFile<Login, Hour, Record, Cursor<Vec<u8>>>;
type SingleFileReader =
    FixedKeyPreallocatedSlotsArchiveReaderSingleFile<Login, Hour, Record, Cursor<Vec<u8>>>;

async fn archive_of(logins: &[u64], capacity_l1: usize) -> InMemoryIo {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 16, capacity_l1)
        .await
        .unwrap();

    for login in logins {
        writer
            .write(Login(*login), Hour(0), &Record::with(*login))
            .await
            .unwrap();
    }

    writer.finish().await.unwrap();
    io
}

#[tokio::test]
async fn get_returns_the_inclusive_range_in_slot_order() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    for hour in [3, 4, 6] {
        writer
            .write(Login(77), Hour(hour), &Record::with(hour as u64))
            .await
            .unwrap();
    }
    writer.finish().await.unwrap();
    let reader = Reader::open(io).await.unwrap();

    let items = reader
        .get(&Login(77), Hour(3), Hour(6))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        items,
        vec![
            Record::with(3),
            Record::with(4),
            Record::default(),
            Record::with(6)
        ]
    );

    let one = reader
        .get(&Login(77), Hour(4), Hour(4))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(one, vec![Record::with(4)]);
}

#[tokio::test]
async fn from_after_to_is_empty_without_reading_and_an_absent_key_is_none() {
    let io = archive_of(&[10, 20, 30], 1024).await;
    let reader = Reader::open(io.clone()).await.unwrap();

    let index = io.file(FileRole::Index).unwrap();
    let data_0 = io.file(FileRole::Data(0)).unwrap();
    let opened = index.read_ats();

    assert_eq!(
        reader.get(&Login(20), Hour(5), Hour(4)).await.unwrap(),
        Some(vec![])
    );
    assert_eq!(
        reader.get(&Login(999), Hour(5), Hour(4)).await.unwrap(),
        Some(vec![])
    );
    assert!(
        reader.l1_cache.pages.read().await.is_empty(),
        "nothing was read"
    );
    assert_eq!(index.read_ats(), opened);
    assert_eq!((data_0.readers_opened(), data_0.read_ats()), (0, 0));

    for login in [0, 15, 31, 999] {
        assert_eq!(
            reader.get(&Login(login), Hour(0), Hour(23)).await.unwrap(),
            None
        );
    }
    assert_eq!(
        data_0.readers_opened(),
        0,
        "an absent key reads no data file"
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
    assert_eq!(reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap(), None);
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

    let io = archive_of(&[5], 1024).await;
    let data = file_bytes(&io, FileRole::Data(0)).await;

    let cut = InMemoryIo::default();
    cut.put(FileRole::Index, file_bytes(&io, FileRole::Index).await);
    cut.put(FileRole::Data(0), data[..data.len() - 100].to_vec());
    let reader = Reader::open(cut.clone()).await.unwrap();
    assert!(matches!(
        reader.get(&Login(5), Hour(0), Hour(0)).await,
        Err(ArchiveError::Corrupted(_))
    ));
    assert_eq!(cut.file(FileRole::Data(0)).unwrap().eof_reads(), 0);

    let reader = Reader::open(io.clone()).await.unwrap();
    io.file(FileRole::Data(0)).unwrap().fail_reads();
    assert!(matches!(
        reader.get(&Login(5), Hour(0), Hour(0)).await,
        Err(ArchiveError::Io(_))
    ));
}

#[tokio::test]
async fn a_hit_stamps_last_access_and_a_full_cache_evicts_the_oldest() {
    let io = archive_of(&[1, 2, 3, 4, 5, 6], 2).await;
    let reader = Reader::open(io).await.unwrap();
    reader.set_data_cache_size(2).await;

    let key = |login| {
        let reader = &reader;
        async move {
            reader
                .data_ref_of(&Login(login))
                .await
                .unwrap()
                .unwrap()
                .cache_key()
        }
    };
    let (a, b, c) = (key(1).await, key(2).await, key(3).await);

    let stamp = |at: u64| {
        let reader = &reader;
        async move {
            reader.data_cache.pages.read().await[&at]
                .last_access
                .get_unix_microseconds()
        }
    };

    reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap();
    let first = stamp(a).await;
    tokio::time::sleep(Duration::from_millis(3)).await;

    reader.get(&Login(2), Hour(0), Hour(0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;

    reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap();
    assert!(stamp(a).await > first, "a hit moves last_access");
    tokio::time::sleep(Duration::from_millis(3)).await;

    reader.get(&Login(3), Hour(0), Hour(0)).await.unwrap();

    let mut cached: Vec<u64> = reader
        .data_cache
        .pages
        .read()
        .await
        .keys()
        .copied()
        .collect();
    cached.sort_unstable();
    let mut expected = vec![a, c];
    expected.sort_unstable();
    assert_eq!(cached, expected, "login 2 was the oldest and went");
    assert!(!cached.contains(&b));

    reader.set_l1_cache_size(1).await;
    reader.get(&Login(6), Hour(0), Hour(0)).await.unwrap();
    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);
}

#[tokio::test]
async fn a_miss_reads_without_holding_a_cache_lock_or_blocking_the_same_file() {
    let io = archive_of(&[1, 2, 3, 4], 2).await;
    let data_0 = io.file(FileRole::Data(0)).unwrap();
    let reader = Arc::new(Reader::open(io).await.unwrap());

    reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap();

    let parked_at = reader.data_ref_of(&Login(4)).await.unwrap().unwrap();
    data_0.shut_reads_at(parked_at.offset as u64);

    let missing = tokio::spawn({
        let reader = reader.clone();
        async move { reader.get(&Login(4), Hour(0), Hour(0)).await.unwrap() }
    });

    wait_until("the miss parking inside its read", || {
        data_0.is_read_waiting()
    })
    .await;

    let hit = tokio::time::timeout(
        Duration::from_secs(2),
        reader.get(&Login(1), Hour(0), Hour(0)),
    )
    .await
    .expect("a cached read waited on a read in flight");
    assert_eq!(hit.unwrap().unwrap(), vec![Record::with(1)]);

    tokio::time::timeout(Duration::from_secs(2), async {
        drop(reader.l1_cache.pages.write().await);
        drop(reader.data_cache.pages.write().await);
    })
    .await
    .expect("a cache lock was held across a read");

    let other = tokio::time::timeout(
        Duration::from_secs(2),
        reader.get(&Login(3), Hour(0), Hour(0)),
    )
    .await
    .expect("a miss of another page of the same data file waited on the parked read");
    assert_eq!(other.unwrap().unwrap(), vec![Record::with(3)]);
    assert!(data_0.is_read_waiting());

    data_0.open_reads();
    assert_eq!(missing.await.unwrap().unwrap(), vec![Record::with(4)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn data_readers_are_opened_lazily_and_once_per_file() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(2 * DATA_PAGE as u64);
    for login in 1..=6u64 {
        writer
            .write(Login(login), Hour(0), &Record::with(login))
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
            .data_ref_of(&Login(3))
            .await
            .unwrap()
            .unwrap()
            .file_no,
        1
    );
    assert_eq!(
        reader.get(&Login(3), Hour(0), Hour(0)).await.unwrap(),
        Some(vec![Record::with(3)])
    );
    assert_eq!(opened(&io), [0, 1, 0]);

    // Concurrent FIRST uses of data file 2, and concurrent later uses of data file 1.
    let tasks: Vec<_> = (0..16u64)
        .map(|task| {
            let reader = reader.clone();
            let login = 3 + task % 4;
            tokio::spawn(async move {
                reader.get(&Login(login), Hour(0), Hour(0)).await.unwrap()
                    == Some(vec![Record::with(login)])
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
    let mut rng = Rng(8);
    let logins = logins(1_100, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    let source = Reader::open(build(&rows, 1024, 1024, 64, None).await)
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

    // The first key's page, then the last key's: the last L1 page ends exactly where the index does.
    for (n, login) in [logins[0], logins[logins.len() - 1]]
        .into_iter()
        .enumerate()
    {
        reader
            .get(&Login(login), Hour(0), Hour(23))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(index.read_ats(), 3 + n, "one read per L1 page");
        assert_eq!(data_0.read_ats(), 1 + n, "one read per data page");
    }

    assert_reads_back(&reader, &day).await;
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
    assert_reads_back(&reader, &archive.day).await;

    // Slot 7 of key 2 straddles the first two frames of its data page.
    assert_eq!(
        reader.get(&Login(2), Hour(7), Hour(9)).await.unwrap(),
        Some(vec![
            Record::with(207),
            Record::default(),
            Record::default()
        ])
    );
    assert_eq!(reader.get(&Login(5), Hour(0), Hour(0)).await.unwrap(), None);

    let plan = reader.compaction_plan().await.unwrap();
    let out = InMemoryIo::default();
    reader.write_compacted_to(&plan, &out).await.unwrap();
    let (index, data) = reference_split_day(&archive.day, DEFAULT_PAGES_PER_FILE);
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

    for login in [1, 2, 3, 10] {
        match reader.get(&Login(login), Hour(0), Hour(23)).await {
            Ok(_) => {}
            Err(ArchiveError::Corrupted(_)) => seen = seen.or(Some("get")),
            Err(other) => panic!("get {login}: {other}"),
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

    let cases: [Mutation; 29] = [
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
        ("L0: more intervals than the frame holds", index, |b| {
            patch_l0_u32(b, HB_L0_0, 8, 2)
        }),
        ("L0: a count patched without its hash", index, |b| {
            patch_u32(b, HB_L0_1 + 8, 0)
        }),
        ("L0: intervals out of order across frames", index, |b| {
            patch_u64(b, HB_L0_1 + 20, 2)
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
        ("L1: a body that is not whole entries", index, |b| {
            patch_u32(b, HB_L1_B, 26)
        }),
        ("L1: more entries than the body holds", index, |b| {
            patch_u32(b, HB_L1_B + 8, 2)
        }),
        ("L1: keys not ascending across frames", index, |b| {
            patch_u64(b, HB_L1_A2 + 12, 2)
        }),
        (
            "L1: a first key other than the interval's from",
            index,
            |b| patch_u64(b, HB_L0_0 + 20, 0),
        ),
        ("L1: a last key other than the interval's to", index, |b| {
            patch_u64(b, HB_L0_0 + 28, 4)
        }),
        ("L1: an l1_offset at the end of the index", index, |b| {
            patch_u32(b, HB_L0_1 + 20 + 16, HB_INDEX_LEN as u32)
        }),
        ("data: a ref offset past the end of its file", index, |b| {
            patch_u32(b, HB_L1_B + 12 + 9, HB_DATA_LEN as u32 + 10)
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
    bytes[HB_L1_B + 12 + 8] = 7;
    io.put(FileRole::Index, bytes);

    let reader = Reader::open(io).await.unwrap();
    assert!(
        matches!(
            reader.get(&Login(10), Hour(0), Hour(0)).await,
            Err(ArchiveError::Io(_))
        ),
        "a ref into a data file that does not exist is the factory's error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_reader_and_compaction_run_inside_spawned_tasks() {
    let mut rng = Rng(314);
    let logins = logins(1_500, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);
    let io = InMemoryIo::default();

    tokio::spawn({
        let io = io.clone();
        async move {
            let mut writer = Writer::new_with_capacity(io, 1024, 64).await?;
            writer.set_l1_cache_size(4);
            writer.set_data_cache_size(100);
            writer.set_max_data_file_bytes(400 * DATA_PAGE as u64);

            for (login, hour, record) in &rows {
                writer.write(Login(*login), Hour(*hour), record).await?;
            }

            writer.finish().await
        }
    })
    .await
    .unwrap()
    .unwrap();

    let reader = Arc::new(Reader::open(io).await.unwrap());
    let day = Arc::new(day);

    let readers: Vec<_> = (0..4)
        .map(|part| {
            let reader = reader.clone();
            let day = day.clone();
            tokio::spawn(async move {
                for (login, hours) in day.iter().skip(part).step_by(4) {
                    let items = reader.get(&Login(*login), Hour(0), Hour(23)).await?;
                    let expected: Vec<Record> = hours
                        .iter()
                        .map(|hour| hour.clone().unwrap_or_default())
                        .collect();
                    assert_eq!(items, Some(expected), "login {login}");
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

    let (index, data) = reference_split_day(&day, DEFAULT_PAGES_PER_FILE);
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
    let mut rng = Rng(2718);

    for count in [0, 3, 5_000] {
        let logins = logins(count, &mut rng);
        let (rows, _) = rows(&logins, &mut rng);

        let mut writer = SingleFileWriter::new(Cursor::new(Vec::new()));
        for (login, hour, record) in &rows {
            writer
                .write(Login(*login), Hour(*hour), record)
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
    let io = archive_of(&[1, 2, 3, 4, 5, 6, 7, 8, 9], 2).await;
    let index = file_bytes(&io, FileRole::Index).await;
    let first_page = l0_frame_len(16);

    for len in [
        first_page,
        first_page + 11,
        index.len() - l1_page_len(2) + 11,
    ] {
        let message = open_error(index[..len].to_vec()).await;
        assert!(message.contains("past the end"), "{len} bytes: {message}");
    }

    let whole_headers = Reader::open(archive_of(&[1, 2, 3], 2).await).await.unwrap();
    assert_eq!(whole_headers.l0_intervals(), 2);
}

#[tokio::test]
async fn flipping_any_bit_of_the_header_of_frame_0_or_of_a_continuation_is_corrupted_at_open() {
    let mut rng = Rng(31);
    let logins = logins(40, &mut rng);
    let (rows, _) = rows(&logins, &mut rng);

    // Two intervals per L0 frame: a chain of many frames.
    let index = file_bytes(&build(&rows, 2, 2, 2, None).await, FileRole::Index).await;
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

/// The compact archive of `count` logins.
async fn compact_logins(count: u64, seed: u64) -> (InMemoryIo, Vec<u64>) {
    let mut rng = Rng(seed);
    let logins = logins(count, &mut rng);
    let (rows, _) = rows(&logins, &mut rng);

    let source = Reader::open(build(&rows, 1024, 1024, 64, None).await)
        .await
        .unwrap();

    (compact(&source).await, logins)
}

#[tokio::test]
async fn an_index_cut_anywhere_inside_its_last_l1_page_is_refused_at_open() {
    // 2,500 keys: three compact L1 pages, the last one ending where the index does.
    let (io, _) = compact_logins(2_500, 99).await;
    let index = file_bytes(&io, FileRole::Index).await;
    let last = l0_frame_len(3) + 2 * l1_page_len(1024);
    assert_eq!(index.len(), last + l1_page_len(1024));

    let (opened, read_ats) = open_index(index.clone()).await;
    assert_eq!(opened, Ok(3));
    assert_eq!(read_ats, 2, "L0, then the header of the last L1 page");

    // A cut at the page's start or inside its header needs no read past L0 to be seen; any cut after the
    // header — what an append that failed partway leaves behind — costs the one read of that header.
    for cut in [
        last,
        last + 11,
        last + 12,
        last + 12 + 3 * 13,
        last + 4_096,
        index.len() - 13,
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

    // A working index is refused alike: nine keys, two to a page, the last page last in the index.
    let working = file_bytes(
        &archive_of(&[1, 2, 3, 4, 5, 6, 7, 8, 9], 2).await,
        FileRole::Index,
    )
    .await;
    let last = working.len() - l1_page_len(2);
    assert_eq!(open_index(working.clone()).await.0, Ok(5));

    for cut in [last + 12, last + 12 + 13, working.len() - 1] {
        let (opened, _) = open_index(working[..cut].to_vec()).await;
        assert!(opened.is_err(), "a working index cut at {cut}: {opened:?}");
    }
}

#[tokio::test]
async fn a_cache_size_set_on_the_reader_evicts_at_once_and_the_caches_stay_within_it() {
    let logins: Vec<u64> = (1..=40).collect();
    let io = archive_of(&logins, 2).await;
    let reader = Reader::open(io).await.unwrap();

    let lens = || async {
        (
            reader.l1_cache.pages.read().await.len(),
            reader.data_cache.pages.read().await.len(),
        )
    };

    let read_all = || async {
        for login in &logins {
            assert_eq!(
                reader.get(&Login(*login), Hour(0), Hour(0)).await.unwrap(),
                Some(vec![Record::with(*login)]),
                "login {login}"
            );
        }
    };

    // Twenty L1 pages and forty data pages read: both caches full at their default of 10.
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
async fn a_resize_landing_while_a_miss_waits_to_insert_leaves_neither_cache_above_it() {
    let logins: Vec<u64> = (1..=40).collect();
    let io = archive_of(&logins, 2).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let index = io.file(FileRole::Index).unwrap();
    let data_0 = io.file(FileRole::Data(0)).unwrap();

    // Both caches full at their default of 10.
    for login in 1..=20 {
        reader.get(&Login(login), Hour(0), Hour(0)).await.unwrap();
    }

    // The L1 page of 40 is read with no lock held, and parks at its insert under the write lock the test
    // holds, while a resize to 1 lands.
    let page_of_40 = reader
        .l0
        .iter()
        .find(|interval| interval.from <= Login(40) && Login(40) <= interval.to)
        .unwrap()
        .l1_offset;
    index.shut_reads_at(page_of_40 as u64);
    {
        let call = reader.get(&Login(40), Hour(0), Hour(0));
        tokio::pin!(call);
        poll_until_parked(call.as_mut(), || index.is_read_waiting()).await;

        let mut pages = reader.l1_cache.pages.write().await;
        index.open_reads();
        assert_still_waiting(call.as_mut()).await;
        reader.l1_cache.resize_locked(&mut pages, 1);
        drop(pages);

        assert_eq!(call.await.unwrap(), Some(vec![Record::with(40)]));
    }
    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);

    // The same for the data page of 39, whose L1 page is the one cached.
    let data_of_39 = reader.data_ref_of(&Login(39)).await.unwrap().unwrap();
    data_0.shut_reads_at(data_of_39.offset as u64);
    {
        let call = reader.get(&Login(39), Hour(0), Hour(0));
        tokio::pin!(call);
        poll_until_parked(call.as_mut(), || data_0.is_read_waiting()).await;

        let mut pages = reader.data_cache.pages.write().await;
        data_0.open_reads();
        assert_still_waiting(call.as_mut()).await;
        reader.data_cache.resize_locked(&mut pages, 1);
        drop(pages);

        assert_eq!(call.await.unwrap(), Some(vec![Record::with(39)]));
    }
    assert_eq!(reader.data_cache.pages.read().await.len(), 1);
    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);
}

#[tokio::test]
async fn two_l0_intervals_naming_one_l1_page_are_an_error_warm_or_cold_never_a_wrong_answer() {
    let (io, logins) = compact_logins(2_500, 17).await;
    let mut index = file_bytes(&io, FileRole::Index).await;

    // L0 entry 1 names page 0 instead of page 1. The hash covers only the header; restamped all the same.
    let entry = |n: usize| l0_frame_len(0) + n * 20 + 16;
    let page_0 = u32::from_le_bytes(index[entry(0)..entry(0) + 4].try_into().unwrap());
    patch_u32(&mut index, entry(1), page_0);
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
                .get(&Login(logins[0]), Hour(0), Hour(0))
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
                    .get(&Login(logins[at]), Hour(0), Hour(23))
                    .await
                    .map(|_| ()),
                &format!("warm {warm}: get of key {at}"),
            );
        }

        for (from, to) in [(0, u64::MAX), (logins[1_000], logins[1_030])] {
            corrupted(
                reader
                    .get_range(&Login(from), &Login(to), Hour(0), Hour(23))
                    .await
                    .map(|_| ()),
                &format!("warm {warm}: range {from}..={to}"),
            );
        }

        // What the damage does not reach still reads.
        for at in [0, 1_023, 2_048, 2_499] {
            assert_eq!(
                reader
                    .get(&Login(logins[at]), Hour(0), Hour(0))
                    .await
                    .unwrap()
                    .map(|items| items.len()),
                Some(1),
                "warm {warm}: key {at}"
            );
        }
    }
}
