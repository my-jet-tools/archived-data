use std::collections::BTreeSet;

use super::split_test_support::{
    DATA_PAGE, DEFAULT_PAGES_PER_FILE, Reader, Row, Writer, assert_reads_back, build, data_files,
    day_of, file_bytes, frame_offsets, l0_frame_len, l1_page_len, logins, parse_index,
    reference_split_day, rows, seeded, write_all,
};
use super::test_types::{Hour, Login, Record, Rng};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::{ArchiveError, DEFAULT_MAX_DATA_FILE_BYTES, DataRef, l0_frame};

async fn write_logins(writer: &mut Writer, logins: &[u64]) {
    for login in logins {
        writer
            .write(Login(*login), Hour(0), &Record::with(*login))
            .await
            .unwrap();
    }
}

fn hour_0_day(logins: impl IntoIterator<Item = u64>) -> super::split_test_support::Day {
    let rows: Vec<Row> = logins
        .into_iter()
        .map(|login| (login, 0, Record::with(login)))
        .collect();
    day_of(&rows)
}

#[tokio::test]
async fn random_insert_orders_with_splits_read_back() {
    for seed in [1u64, 7, 42, 2026] {
        let mut rng = seeded(seed);
        let logins = logins(300, &mut rng);
        let (rows, day) = rows(&logins, &mut rng);

        // Capacity 4 splits constantly; cache 2 forces evictions and read-backs of both page kinds; three
        // data pages per file rolls the data over a hundred times.
        let io = build(&rows, 1024, 4, 2, Some(3)).await;
        let reader = Reader::open(io.clone()).await.unwrap();

        assert!(reader.l0_intervals() > 1, "the test must split");
        assert_eq!(data_files(&io).await.len(), day.len().div_ceil(3));
        assert_eq!(reader.keys_count().await.unwrap(), day.len());

        let parsed = parse_index(&file_bytes(&io, FileRole::Index).await, 4);
        assert_eq!(parsed.counts().iter().sum::<usize>(), day.len());

        assert_reads_back(&reader, &day).await;
        assert_eq!(reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap(), None);
    }
}

#[tokio::test]
async fn an_l1_capacity_of_one_splits_into_one_key_per_page() {
    let mut rng = Rng(99);
    let (rows, day) = rows(&[5, 1, 9, 3, 7], &mut rng);

    let io = build(&rows, 8, 1, 1, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();

    assert_eq!(reader.l0_intervals(), 5);
    assert_eq!(
        parse_index(&file_bytes(&io, FileRole::Index).await, 1).counts(),
        [1; 5]
    );
    assert_reads_back(&reader, &day).await;
}

#[tokio::test]
async fn the_writer_reports_its_keys_and_intervals() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 16, 2).await.unwrap();
    assert_eq!(writer.max_data_file_bytes(), DEFAULT_MAX_DATA_FILE_BYTES);

    for (login, value) in [(10, 1), (20, 2), (30, 3), (20, 4), (10, 5)] {
        writer
            .write(Login(login), Hour(1), &Record::with(value))
            .await
            .unwrap();
    }

    assert_eq!(writer.keys_count(), 3);
    assert_eq!(writer.l0_intervals(), 2);
    assert_eq!(writer.data_files(), 1);
    writer.finish().await.unwrap();

    assert_eq!(
        io.file(FileRole::Data(0)).unwrap().appends(),
        3,
        "a known key gets no second page"
    );

    let reader = Reader::open(io).await.unwrap();
    for (login, value) in [(10, 5), (20, 4), (30, 3)] {
        assert_eq!(
            reader.get(&Login(login), Hour(1), Hour(1)).await.unwrap(),
            Some(vec![Record::with(value)]),
            "a known key is overwritten in place"
        );
    }
}

#[tokio::test]
async fn l0_chains_across_several_frames_and_reads_back() {
    for seed in [3u64, 11, 500] {
        let mut rng = seeded(seed);
        let logins = logins(400, &mut rng);
        let (rows, day) = rows(&logins, &mut rng);

        // Two intervals per frame and two keys per L1 page: over a hundred frames.
        let io = build(&rows, 2, 2, 2, None).await;
        let index = file_bytes(&io, FileRole::Index).await;
        let reader = Reader::open(io).await.unwrap();

        assert!(reader.l0_frames() > 3, "the test must chain");
        assert_eq!(reader.l0_frames(), reader.l0_intervals().div_ceil(2));
        assert_eq!(frame_offsets(&index).len(), reader.l0_frames());
        assert_eq!(reader.keys_count().await.unwrap(), day.len());
        assert_reads_back(&reader, &day).await;

        let out = super::split_test_support::compact(&reader).await;
        let (reference_index, reference_data) = reference_split_day(&day, DEFAULT_PAGES_PER_FILE);
        assert!(
            file_bytes(&out, FileRole::Index).await == reference_index,
            "a chain compacts to one frame"
        );
        assert!(data_files(&out).await == reference_data);
        assert_eq!(Reader::open(out).await.unwrap().l0_frames(), 1);
    }
}

#[tokio::test]
async fn continuations_are_reused_by_every_later_flush() {
    let mut rng = Rng(77);
    let logins = logins(300, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 3, 2).await.unwrap();
    writer.set_data_cache_size(4);

    let mut frames = 0;
    let mut written = 0;

    for chunk in rows.chunks(rows.len() / 5 + 1) {
        write_all(&mut writer, chunk).await;
        writer.flush().await.unwrap();
        written += chunk.len();

        assert!(writer.l0_frames() >= frames, "a frame is never given back");
        assert_eq!(writer.l0_frames(), writer.l0_intervals().div_ceil(3));
        frames = writer.l0_frames();

        // What a flush returned is readable while the writer goes on.
        let so_far = Reader::open(io.clone()).await.unwrap();
        assert_eq!(so_far.l0_frames(), frames);
        assert_reads_back(&so_far, &day_of(&rows[..written])).await;
    }

    writer.finish().await.unwrap();

    let offsets = frame_offsets(&file_bytes(&io, FileRole::Index).await);
    assert_eq!(offsets.len(), frames);
    assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));

    let reader = Reader::open(io).await.unwrap();
    assert_reads_back(&reader, &day).await;
}

#[tokio::test]
async fn a_slot_out_of_range_is_refused_on_write_and_read() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();

    let result = writer.write(Login(1), Hour(24), &Record::default()).await;
    assert!(matches!(result, Err(ArchiveError::ContractViolation(_))));
    assert_eq!(io.created_roles(), [FileRole::Index], "nothing was placed");

    writer
        .write(Login(1), Hour(0), &Record::with(1))
        .await
        .unwrap();
    writer.finish().await.unwrap();
    let reader = Reader::open(io).await.unwrap();

    assert!(matches!(
        reader.get(&Login(1), Hour(0), Hour(24)).await,
        Err(ArchiveError::ContractViolation(_))
    ));
    assert_eq!(
        reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap(),
        Some(vec![Record::with(1)])
    );
}

#[tokio::test]
async fn ascending_writes_fill_every_l1_page_but_the_last() {
    for (capacity, keys, l1_cache) in [(4usize, 10u64, 64usize), (7, 100, 1)] {
        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), 1024, capacity)
            .await
            .unwrap();
        writer.set_l1_cache_size(l1_cache);

        let logins: Vec<u64> = (1..=keys).collect();
        write_logins(&mut writer, &logins).await;
        writer.finish().await.unwrap();

        let pages = (keys as usize).div_ceil(capacity);
        let mut expected = vec![capacity; pages];
        expected[pages - 1] = keys as usize - (pages - 1) * capacity;

        let index = file_bytes(&io, FileRole::Index).await;
        assert_eq!(parse_index(&index, capacity).counts(), expected);
        assert_eq!(
            index.len(),
            l0_frame_len(1024) + pages * l1_page_len(capacity),
            "the index holds exactly ceil(n / capacity) L1 pages"
        );

        let reader = Reader::open(io).await.unwrap();
        assert_reads_back(&reader, &hour_0_day(logins)).await;
    }
}

/// The keys of every L1 page after writing `logins` in order at capacity 4, and the index's length.
async fn pages_after(logins: &[u64]) -> (Vec<Vec<u64>>, usize) {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 16, 4).await.unwrap();
    write_logins(&mut writer, logins).await;
    writer.finish().await.unwrap();

    let index = file_bytes(&io, FileRole::Index).await;
    let keys = parse_index(&index, 4).keys();

    let reader = Reader::open(io).await.unwrap();
    assert_reads_back(&reader, &hour_0_day(logins.iter().copied())).await;

    (keys, index.len())
}

#[tokio::test]
async fn a_key_below_the_last_pages_max_still_halves() {
    let (pages, _) = pages_after(&[10, 20, 30, 40, 35]).await;
    assert_eq!(pages, [vec![10, 20], vec![30, 35, 40]]);
}

#[tokio::test]
async fn an_overflow_halves_into_the_next_page_when_it_has_room() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 16, 4).await.unwrap();

    write_logins(&mut writer, &[10, 20, 30, 40, 50]).await;
    assert_eq!(writer.l0_intervals(), 2);
    write_logins(&mut writer, &[25]).await;
    assert_eq!(writer.l0_intervals(), 2, "no interval was added");
    writer.finish().await.unwrap();

    let index = file_bytes(&io, FileRole::Index).await;
    assert_eq!(
        parse_index(&index, 4).keys(),
        [vec![10, 20], vec![25, 30, 40, 50]]
    );
    assert_eq!(
        index.len(),
        l0_frame_len(16) + 2 * l1_page_len(4),
        "no page was allocated"
    );

    let (pages, len) = pages_after(&[10, 20, 30, 40, 50, 25]).await;
    assert_eq!(pages, [vec![10, 20], vec![25, 30, 40, 50]]);
    assert_eq!(len, index.len());
}

#[tokio::test]
async fn an_overflow_allocates_a_page_when_the_next_one_has_no_room() {
    let (pages, len) = pages_after(&[10, 20, 30, 40, 50, 60, 25]).await;

    assert_eq!(pages, [vec![10, 20], vec![25, 30, 40], vec![50, 60]]);
    assert_eq!(len, l0_frame_len(16) + 3 * l1_page_len(4));
}

#[tokio::test]
async fn descending_and_random_orders_keep_every_index_invariant() {
    for seed in [5u64, 99] {
        let mut rng = seeded(seed);
        let logins = logins(60, &mut rng);
        let (random, day) = rows(&logins, &mut rng);

        let mut ascending = random.clone();
        ascending.sort_by_key(|(login, hour, _)| (*login, *hour));
        let mut descending = ascending.clone();
        descending.reverse();

        for (order, rows) in [
            ("ascending", &ascending),
            ("descending", &descending),
            ("random", &random),
        ] {
            for capacity in [1, 2, 4, 16] {
                for cache in [1, 3] {
                    let case = format!("{order}, seed {seed}, capacity {capacity}, cache {cache}");

                    // Four intervals per L0 frame and five data pages per file: L0 chains, data rolls over.
                    let io = build(rows, 4, capacity, cache, Some(5)).await;
                    let parsed = parse_index(&file_bytes(&io, FileRole::Index).await, capacity);
                    let counts = parsed.counts();

                    assert_eq!(counts.iter().sum::<usize>(), day.len(), "{case}");
                    if order == "ascending" {
                        assert!(
                            counts[..counts.len() - 1]
                                .iter()
                                .all(|count| *count == capacity),
                            "{case}: {counts:?}"
                        );
                    }

                    let refs: BTreeSet<DataRef> = parsed.refs().into_iter().collect();
                    assert_eq!(refs.len(), day.len(), "{case}: every key has its own page");

                    let reader = Reader::open(io).await.unwrap();
                    assert_eq!(reader.keys_count().await.unwrap(), day.len(), "{case}");
                    assert_reads_back(&reader, &day).await;
                }
            }
        }
    }
}

#[tokio::test]
async fn data_pages_roll_over_to_the_next_file_at_max_data_file_bytes() {
    let mut rng = Rng(12);
    let (rows, day) = rows(&[70, 10, 50, 30, 60, 20, 40], &mut rng);

    for max in [2 * DATA_PAGE + 100, 2 * DATA_PAGE] {
        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), 1024, 2)
            .await
            .unwrap();
        writer.set_max_data_file_bytes(max as u64);
        writer.set_data_cache_size(3);
        assert_eq!(writer.max_data_file_bytes(), max as u64);

        write_all(&mut writer, &rows).await;
        assert_eq!(writer.data_files(), 4);
        writer.finish().await.unwrap();

        let sizes: Vec<usize> = data_files(&io).await.iter().map(Vec::len).collect();
        assert_eq!(sizes, [6164, 6164, 6164, 3082], "max {max}");

        let refs: BTreeSet<DataRef> = parse_index(&file_bytes(&io, FileRole::Index).await, 2)
            .refs()
            .into_iter()
            .collect();
        let whole: BTreeSet<DataRef> = (0..7)
            .map(|rank| DataRef {
                file_no: (rank / 2) as u8,
                offset: (rank % 2 * DATA_PAGE) as u32,
            })
            .collect();
        assert_eq!(refs, whole, "every page lies whole inside its data file");

        let reader = Reader::open(io).await.unwrap();
        assert_reads_back(&reader, &day).await;
    }
}

#[tokio::test]
async fn a_known_key_in_an_earlier_data_file_is_overwritten_in_place() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);
    writer.set_data_cache_size(0);

    write_logins(&mut writer, &[1, 2, 3]).await;
    assert_eq!(writer.data_files(), 3);

    let data_0 = io.file(FileRole::Data(0)).unwrap();
    assert_eq!((data_0.appends(), data_0.write_ats()), (1, 1));

    writer
        .write(Login(1), Hour(5), &Record::with(15))
        .await
        .unwrap();
    writer
        .write(Login(1), Hour(0), &Record::with(10))
        .await
        .unwrap();

    assert_eq!((data_0.appends(), data_0.write_ats()), (1, 3));
    assert_eq!(data_0.bytes().await.len(), DATA_PAGE);
    writer.finish().await.unwrap();

    assert_eq!(
        io.roles(),
        [
            FileRole::Index,
            FileRole::Data(0),
            FileRole::Data(1),
            FileRole::Data(2)
        ]
    );

    let reader = Reader::open(io).await.unwrap();
    let mut expected = vec![Record::default(); 6];
    expected[0] = Record::with(10);
    expected[5] = Record::with(15);
    assert_eq!(
        reader.get(&Login(1), Hour(0), Hour(5)).await.unwrap(),
        Some(expected)
    );
}

#[tokio::test]
async fn the_257th_data_file_is_a_contract_violation_and_the_writer_stays_usable() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);

    for login in 0..256u64 {
        writer
            .write(Login(login), Hour(1), &Record::with(login))
            .await
            .unwrap();
    }

    for _ in 0..2 {
        assert!(matches!(
            writer.write(Login(256), Hour(1), &Record::with(256)).await,
            Err(ArchiveError::ContractViolation(_))
        ));
        assert_eq!(writer.keys_count(), 256);
        assert_eq!(writer.data_files(), 256);
    }

    writer
        .write(Login(7), Hour(2), &Record::with(72))
        .await
        .unwrap();
    writer.finish().await.unwrap();
    assert_eq!(io.roles().len(), 257);

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), 256);

    for login in 0..256u64 {
        assert_eq!(
            reader.get(&Login(login), Hour(1), Hour(1)).await.unwrap(),
            Some(vec![Record::with(login)])
        );
    }
    assert_eq!(
        reader.get(&Login(7), Hour(2), Hour(2)).await.unwrap(),
        Some(vec![Record::with(72)])
    );
    assert_eq!(
        reader.get(&Login(256), Hour(1), Hour(1)).await.unwrap(),
        None
    );
}

#[tokio::test]
#[should_panic(expected = "at least one data page")]
async fn a_data_file_smaller_than_a_page_panics() {
    let mut writer = Writer::new(InMemoryIo::default()).await.unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64 - 1);
}

#[tokio::test]
#[should_panic(expected = "4 GiB")]
async fn a_data_file_past_four_gib_panics() {
    let mut writer = Writer::new(InMemoryIo::default()).await.unwrap();
    writer.set_max_data_file_bytes((1 << 32) + 1);
}

#[tokio::test]
#[should_panic(expected = "once a key is written")]
async fn the_data_file_size_cannot_change_after_the_first_key() {
    let mut writer = Writer::new(InMemoryIo::default()).await.unwrap();
    writer
        .write(Login(1), Hour(0), &Record::with(1))
        .await
        .unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);
}

#[tokio::test]
async fn files_are_created_only_when_first_needed() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 16, 2).await.unwrap();
    writer.set_max_data_file_bytes(2 * DATA_PAGE as u64);
    writer.set_l1_cache_size(1);
    assert_eq!(io.created_roles(), [FileRole::Index]);

    write_logins(&mut writer, &[1, 2]).await;
    assert_eq!(io.created_roles(), [FileRole::Index, FileRole::Data(0)]);

    write_logins(&mut writer, &[3]).await;
    assert_eq!(
        io.created_roles(),
        [FileRole::Index, FileRole::Data(0), FileRole::Data(1)],
        "data file 1 comes with its first page"
    );

    let index = io.file(FileRole::Index).unwrap();
    assert_eq!(index.readers_opened(), 0, "every L1 page so far was cached");

    // Page [1, 2] was evicted by page [3]: reading it back opens the index's reader, once.
    writer
        .write(Login(1), Hour(1), &Record::with(11))
        .await
        .unwrap();
    assert_eq!(index.readers_opened(), 1);

    writer
        .write(Login(3), Hour(1), &Record::with(31))
        .await
        .unwrap();
    assert_eq!(index.readers_opened(), 1);
    assert_eq!(index.read_ats(), 2);

    writer.finish().await.unwrap();

    assert_eq!(io.created_roles().len(), 3);
    for no in 0..2 {
        assert_eq!(
            io.file(FileRole::Data(no)).unwrap().readers_opened(),
            0,
            "the writer never reads a data file"
        );
    }

    let mut day = hour_0_day([1, 2, 3]);
    day.get_mut(&1).unwrap()[1] = Some(Record::with(11));
    day.get_mut(&3).unwrap()[1] = Some(Record::with(31));
    assert_reads_back(&Reader::open(io).await.unwrap(), &day).await;
}

#[tokio::test]
async fn finish_finishes_every_writer_even_when_one_fails() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);
    write_logins(&mut writer, &[1, 2, 3]).await;

    io.file(FileRole::Data(1)).unwrap().fail_finish();
    assert!(matches!(writer.finish().await, Err(ArchiveError::Io(_))));

    for role in [FileRole::Index, FileRole::Data(0), FileRole::Data(2)] {
        let file = io.file(role).unwrap();
        assert_eq!(
            (file.finish_attempts(), file.finishes()),
            (1, 1),
            "{role:?}"
        );
    }

    let failed = io.file(FileRole::Data(1)).unwrap();
    assert_eq!((failed.finish_attempts(), failed.finishes()), (1, 0));

    for role in io.roles() {
        assert_eq!(io.file(role).unwrap().finishes_spawned_by_drop(), 0);
    }
}

#[tokio::test]
async fn finish_after_a_failed_flush_returns_the_flush_error_and_still_finishes_every_file() {
    for failing in ["the index's write_at", "a data file's append"] {
        let io = InMemoryIo::default();
        let mut writer = Writer::new(io.clone()).await.unwrap();
        writer.set_max_data_file_bytes(DATA_PAGE as u64);
        write_logins(&mut writer, &[1, 2, 3]).await;

        match failing {
            "the index's write_at" => io.file(FileRole::Index).unwrap().fail_write_ats(),
            _ => io.file(FileRole::Data(1)).unwrap().fail_appends(),
        }
        io.file(FileRole::Data(2)).unwrap().fail_finish();

        let message = match writer.finish().await {
            Err(ArchiveError::Io(err)) => err.to_string(),
            other => panic!("{failing}: {other:?}"),
        };
        assert!(
            message.contains("set to fail") && !message.contains("finish"),
            "{failing}: the flush's error comes first, not {message}"
        );

        for role in io.roles() {
            let file = io.file(role).unwrap();
            let finished = usize::from(role != FileRole::Data(2));

            assert_eq!(
                (
                    file.finish_attempts(),
                    file.finishes(),
                    file.finishes_spawned_by_drop()
                ),
                (1, finished, 0),
                "{failing}: {role:?}"
            );
        }
        assert_eq!(io.roles().len(), 4, "{failing}");
    }
}

#[tokio::test]
async fn finish_of_a_clean_archive_finishes_each_file_exactly_once() {
    let empty = InMemoryIo::default();
    Writer::new(empty.clone())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();

    assert_eq!(empty.roles(), [FileRole::Index]);
    assert_eq!(
        file_bytes(&empty, FileRole::Index).await,
        l0_frame(l0_frame_len(1024) as u32, 0, 0, &[])
    );

    let mut rng = Rng(18);
    let logins = logins(20, &mut rng);
    let (rows, _) = rows(&logins, &mut rng);
    let io = build(&rows, 1024, 4, 2, Some(4)).await;

    for archive in [&empty, &io] {
        for role in archive.roles() {
            let file = archive.file(role).unwrap();
            assert_eq!(
                (
                    file.finish_attempts(),
                    file.finishes(),
                    file.finishes_spawned_by_drop()
                ),
                (1, 1, 0),
                "{role:?}"
            );
        }
    }
    assert_eq!(io.roles().len(), 1 + 5);
}
