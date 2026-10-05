use std::collections::BTreeSet;

use super::split_test_support::{
    DATA_PAGE, DEFAULT_PAGES_PER_FILE, Reader, Writer, assert_reads_back, build, compact,
    data_files, file_bytes, frame_offsets, hour_0_rows, key, names_of, numbered_keys, parse_index,
    random_keys, reference_split_compact, rows, seeded, write_all,
};
use super::test_types::{Hour, Name, Names, Record};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::{ArchiveError, DEFAULT_MAX_DATA_FILE_BYTES, DataRef, l0_frame};

fn keys_of(numbers: &[u64]) -> Vec<Vec<u8>> {
    numbers.iter().copied().map(key).collect()
}

async fn write_keys(writer: &mut Writer, keys: &[Vec<u8>]) {
    write_all(writer, &hour_0_rows(keys)).await;
}

#[tokio::test]
async fn random_insert_orders_and_key_lengths_with_splits_read_back() {
    for seed in [1u64, 7, 42, 2026] {
        let mut rng = seeded(seed);
        let keys = random_keys(300, 300, &mut rng);
        let (rows, names) = rows(&keys, &mut rng);

        // 1 KiB pages split constantly and hold one to three intervals per L0 frame; cache 2 forces
        // evictions and read-backs of both page kinds; three data pages per file rolls the data over.
        let io = build(&rows, 1032, 1024, 2, Some(3)).await;
        let index = file_bytes(&io, FileRole::Index).await;
        let reader = Reader::open(io.clone()).await.unwrap();

        assert!(reader.l0_intervals() > 10, "the test must split");
        assert!(reader.l0_frames() > 3, "the test must chain");
        assert_eq!(frame_offsets(&index).len(), reader.l0_frames());
        assert_eq!(data_files(&io).await.len(), names.len().div_ceil(3));
        assert_eq!(reader.keys_count().await.unwrap(), names.len());

        let parsed = parse_index(&index, Some(1024));
        assert_eq!(parsed.counts().iter().sum::<usize>(), names.len());

        assert_reads_back(&reader, &names).await;

        for absent in random_keys(50, 300, &mut rng) {
            if !names.contains_key(&absent) {
                assert_eq!(
                    reader.get(&Name(absent), Hour(0), Hour(23)).await.unwrap(),
                    None
                );
            }
        }
    }
}

#[tokio::test]
async fn empty_and_over_half_page_keys_are_refused_before_anything_is_written() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();

    // A default L1 page is 16 KiB: an entry may be (16384 − 12) / 2 = 8186 bytes, so 2 + 8179 + 5.
    for key in [vec![], vec![7u8; 8180], vec![7u8; 20_000]] {
        let len = key.len();
        assert!(
            matches!(
                writer.write(Name(key), Hour(0), &Record::with(1)).await,
                Err(ArchiveError::ContractViolation(_))
            ),
            "a key of {len} bytes was accepted"
        );
    }
    assert_eq!(writer.keys_count(), 0);
    assert_eq!(writer.l0_intervals(), 0);
    assert_eq!(io.created_roles(), [FileRole::Index], "nothing was placed");
    assert_eq!(
        file_bytes(&io, FileRole::Index).await.len(),
        64 * 1024,
        "the index holds frame 0 alone"
    );

    writer
        .write(Name(vec![7u8; 8179]), Hour(2), &Record::with(2))
        .await
        .unwrap();
    writer
        .write(Name(vec![1]), Hour(3), &Record::with(3))
        .await
        .unwrap();
    writer.finish().await.unwrap();

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), 2);
    assert_eq!(
        reader
            .get(&Name(vec![7u8; 8179]), Hour(2), Hour(2))
            .await
            .unwrap(),
        Some(vec![Record::with(2)])
    );
}

#[tokio::test]
async fn the_largest_key_is_32767_bytes_and_compacts_into_a_page_of_its_own() {
    // Pages large enough that only the length limit, not the half-page rule, decides.
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 1 << 17, 1 << 17)
        .await
        .unwrap();

    assert!(matches!(
        writer
            .write(Name(vec![9u8; 32768]), Hour(0), &Record::with(1))
            .await,
        Err(ArchiveError::ContractViolation(_))
    ));

    let mut names = Names::new();
    for (key, hour) in [
        (vec![9u8; 32767], 4),
        (vec![8u8; 128], 5),
        (vec![8u8; 127], 6),
        (vec![3], 7),
    ] {
        writer
            .write(Name(key.clone()), Hour(hour), &Record::with(hour as u64))
            .await
            .unwrap();
        names.entry(key).or_insert_with(|| vec![None; 24])[hour] = Some(Record::with(hour as u64));
    }
    writer.finish().await.unwrap();

    let reader = Reader::open(io).await.unwrap();
    assert_reads_back(&reader, &names).await;

    let out = compact(&reader).await;
    let (index, data) = reference_split_compact(&names, DEFAULT_PAGES_PER_FILE);
    assert!(file_bytes(&out, FileRole::Index).await == index);
    assert!(data_files(&out).await == data);

    let parsed = parse_index(&index, None);
    assert_eq!(
        (
            *parsed.sizes.last().unwrap(),
            *parsed.counts().last().unwrap()
        ),
        (12 + 2 + 32767 + 5, 1),
        "an entry past 16 KiB still gets a page"
    );
    assert_reads_back(&Reader::open(out).await.unwrap(), &names).await;
}

#[tokio::test]
async fn a_split_by_bytes_leaves_two_pages_that_fit() {
    let mut rng = seeded(64);

    // A 64-byte page takes 52 bytes of entries, and an entry may be 26: a 20-byte key.
    let mut keys: Vec<Vec<u8>> = (0..60u8).map(|i| vec![i; 20]).collect();
    keys.extend((60..120u8).map(|i| vec![i; 3]));
    let (rows, names) = rows(&keys, &mut rng);

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 72, 64).await.unwrap();
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);

    assert!(matches!(
        writer
            .write(Name(vec![0; 21]), Hour(0), &Record::default())
            .await,
        Err(ArchiveError::ContractViolation(_))
    ));

    write_all(&mut writer, &rows).await;
    writer.finish().await.unwrap();

    // Every page is 64 bytes: at most two 26-byte entries, at most five 9-byte ones.
    let parsed = parse_index(&file_bytes(&io, FileRole::Index).await, Some(64));
    assert!(parsed.counts().len() > 20, "the test must split");
    assert!(parsed.counts().iter().all(|count| (1..=5).contains(count)));
    assert!(parsed.entry_bytes().iter().all(|bytes| *bytes <= 52));

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), names.len());
    assert_reads_back(&reader, &names).await;
}

#[tokio::test]
async fn the_writer_reports_its_keys_and_overwrites_a_known_key_in_place() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 72, 64).await.unwrap();
    assert_eq!(writer.max_data_file_bytes(), DEFAULT_MAX_DATA_FILE_BYTES);

    for (key, value) in [
        (&b"alpha-key"[..], 1),
        (b"bravo-key", 2),
        (b"delta-key", 3),
        (b"bravo-key", 4),
        (b"alpha-key", 5),
        (b"charl-key", 6),
    ] {
        writer
            .write(Name(key.to_vec()), Hour(1), &Record::with(value))
            .await
            .unwrap();
    }

    // Three 15-byte entries fill a 64-byte page; the fourth key, below the last, splits it.
    assert_eq!(writer.keys_count(), 4);
    assert_eq!(writer.l0_intervals(), 2);
    assert_eq!(writer.data_files(), 1);
    writer.finish().await.unwrap();

    assert_eq!(
        io.file(FileRole::Data(0)).unwrap().appends(),
        4,
        "a known key gets no second page"
    );

    let reader = Reader::open(io).await.unwrap();
    for (key, value) in [
        (&b"alpha-key"[..], 5),
        (b"bravo-key", 4),
        (b"charl-key", 6),
        (b"delta-key", 3),
    ] {
        assert_eq!(
            reader
                .get(&Name(key.to_vec()), Hour(1), Hour(1))
                .await
                .unwrap(),
            Some(vec![Record::with(value)]),
            "a known key is overwritten in place"
        );
    }
}

#[tokio::test]
async fn l0_chains_across_several_frames_and_reads_back() {
    for seed in [3u64, 11, 500] {
        let mut rng = seeded(seed);
        let (rows, names) = rows(&numbered_keys(400), &mut rng);

        // A 24-byte interval, two to a 68-byte frame; three keys to a 64-byte L1 page.
        let io = build(&rows, 68, 64, 2, None).await;
        let index = file_bytes(&io, FileRole::Index).await;
        let reader = Reader::open(io).await.unwrap();

        assert!(reader.l0_frames() > 3, "the test must chain");
        assert_eq!(reader.l0_frames(), reader.l0_intervals().div_ceil(2));
        assert_eq!(frame_offsets(&index).len(), reader.l0_frames());
        assert_eq!(reader.keys_count().await.unwrap(), names.len());
        assert_reads_back(&reader, &names).await;

        let out = compact(&reader).await;
        let (reference_index, reference_data) =
            reference_split_compact(&names, DEFAULT_PAGES_PER_FILE);
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
    let mut rng = seeded(77);
    let (rows, names) = rows(&numbered_keys(300), &mut rng);

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 68, 64).await.unwrap();
    writer.set_data_cache_size(4);

    let mut frames = 0;
    let mut written = 0;

    for chunk in rows.chunks(rows.len() / 5 + 1) {
        write_all(&mut writer, chunk).await;
        writer.flush().await.unwrap();
        written += chunk.len();

        assert!(writer.l0_frames() >= frames, "a frame is never given back");
        assert_eq!(writer.l0_frames(), writer.l0_intervals().div_ceil(2));
        frames = writer.l0_frames();

        // What a flush returned is readable while the writer goes on.
        let so_far = Reader::open(io.clone()).await.unwrap();
        assert_eq!(so_far.l0_frames(), frames);
        assert_reads_back(&so_far, &names_of(&rows[..written])).await;
    }

    writer.finish().await.unwrap();

    let offsets = frame_offsets(&file_bytes(&io, FileRole::Index).await);
    assert_eq!(offsets.len(), frames);
    assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));

    let reader = Reader::open(io).await.unwrap();
    assert_reads_back(&reader, &names).await;
}

#[tokio::test]
async fn a_slot_out_of_range_is_refused_on_write_and_read() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();

    let result = writer
        .write(Name(b"one".to_vec()), Hour(24), &Record::default())
        .await;
    assert!(matches!(result, Err(ArchiveError::ContractViolation(_))));
    assert_eq!(io.created_roles(), [FileRole::Index], "nothing was placed");

    writer
        .write(Name(b"one".to_vec()), Hour(0), &Record::with(1))
        .await
        .unwrap();
    writer.finish().await.unwrap();
    let reader = Reader::open(io).await.unwrap();

    assert!(matches!(
        reader.get(&Name(b"one".to_vec()), Hour(0), Hour(24)).await,
        Err(ArchiveError::ContractViolation(_))
    ));
    assert_eq!(
        reader
            .get(&Name(b"one".to_vec()), Hour(0), Hour(0))
            .await
            .unwrap(),
        Some(vec![Record::with(1)])
    );
}

#[tokio::test]
async fn ascending_writes_fill_every_l1_page_but_the_last() {
    // A 9-byte key is a 15-byte entry: three fill the 52 bytes of a 64-byte page.
    for (count, l1_cache) in [(10u64, 64usize), (100, 1)] {
        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), 1024, 64)
            .await
            .unwrap();
        writer.set_l1_cache_size(l1_cache);

        let keys = numbered_keys(count);
        write_keys(&mut writer, &keys).await;
        writer.finish().await.unwrap();

        let pages = (count as usize).div_ceil(3);
        let mut expected = vec![3; pages];
        expected[pages - 1] = count as usize - (pages - 1) * 3;

        let index = file_bytes(&io, FileRole::Index).await;
        assert_eq!(parse_index(&index, Some(64)).counts(), expected);
        assert_eq!(
            index.len(),
            1024 + pages * 64,
            "the index holds exactly ceil(n / 3) L1 pages"
        );

        let reader = Reader::open(io).await.unwrap();
        assert_reads_back(&reader, &names_of(&hour_0_rows(&keys))).await;
    }
}

/// The keys of every L1 page after writing `numbers` in order into 64-byte pages, and the index's length.
async fn pages_after(numbers: &[u64]) -> (Vec<Vec<Vec<u8>>>, usize) {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 1024, 64)
        .await
        .unwrap();
    let keys = keys_of(numbers);
    write_keys(&mut writer, &keys).await;
    writer.finish().await.unwrap();

    let index = file_bytes(&io, FileRole::Index).await;
    let pages = parse_index(&index, Some(64)).keys();

    let reader = Reader::open(io).await.unwrap();
    assert_reads_back(&reader, &names_of(&hour_0_rows(&keys))).await;

    (pages, index.len())
}

fn pages_of(numbers: &[&[u64]]) -> Vec<Vec<Vec<u8>>> {
    numbers.iter().map(|page| keys_of(page)).collect()
}

#[tokio::test]
async fn a_key_below_the_last_pages_max_still_splits() {
    let (pages, len) = pages_after(&[10, 20, 30, 25]).await;
    assert_eq!(pages, pages_of(&[&[10, 20, 25], &[30]]));
    assert_eq!(len, 1024 + 2 * 64);
}

#[tokio::test]
async fn an_overflow_splits_into_the_next_page_when_it_has_room() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 1024, 64)
        .await
        .unwrap();

    write_keys(&mut writer, &keys_of(&[10, 20, 30, 50])).await;
    assert_eq!(writer.l0_intervals(), 2);
    write_keys(&mut writer, &keys_of(&[25])).await;
    assert_eq!(writer.l0_intervals(), 2, "no interval was added");
    writer.finish().await.unwrap();

    let index = file_bytes(&io, FileRole::Index).await;
    assert_eq!(
        parse_index(&index, Some(64)).keys(),
        pages_of(&[&[10, 20, 25], &[30, 50]])
    );
    assert_eq!(index.len(), 1024 + 2 * 64, "no page was allocated");

    let (pages, len) = pages_after(&[10, 20, 30, 50, 25]).await;
    assert_eq!(pages, pages_of(&[&[10, 20, 25], &[30, 50]]));
    assert_eq!(len, index.len());
}

#[tokio::test]
async fn an_overflow_allocates_a_page_when_the_next_one_has_no_room() {
    let (pages, len) = pages_after(&[10, 20, 30, 50, 60, 70, 25]).await;

    assert_eq!(pages, pages_of(&[&[10, 20, 25], &[30], &[50, 60, 70]]));
    assert_eq!(len, 1024 + 3 * 64);
}

#[tokio::test]
async fn descending_and_random_orders_keep_every_index_invariant() {
    for seed in [5u64, 99] {
        let mut rng = seeded(seed);
        let keys = random_keys(60, 12, &mut rng);
        let (random, names) = rows(&keys, &mut rng);

        let mut ascending = random.clone();
        ascending.sort_by(|(a, hour_a, _), (b, hour_b, _)| (a, hour_a).cmp(&(b, hour_b)));
        let mut descending = ascending.clone();
        descending.reverse();

        for (order, rows) in [
            ("ascending", &ascending),
            ("descending", &descending),
            ("random", &random),
        ] {
            // (L0 frame, L1 page) in bytes: from one or two keys a page to all of them, L0 chained.
            for (l0, l1) in [(64, 48), (72, 64), (136, 128), (1032, 1024)] {
                for cache in [1, 3] {
                    let case = format!("{order}, seed {seed}, pages {l0}/{l1}, cache {cache}");

                    // Five data pages per file: the data rolls over.
                    let io = build(rows, l0, l1, cache, Some(5)).await;
                    let parsed = parse_index(&file_bytes(&io, FileRole::Index).await, Some(l1));
                    let counts = parsed.counts();

                    assert_eq!(counts.iter().sum::<usize>(), names.len(), "{case}");

                    if order == "ascending" {
                        let entry_bytes = parsed.entry_bytes();
                        let keys = parsed.keys();

                        for page in 0..keys.len() - 1 {
                            let next = variable_entry(&keys[page + 1][0]);
                            assert!(
                                entry_bytes[page] + next > l1 - 12,
                                "{case}: page {page} of {counts:?} had room for the next key"
                            );
                        }
                    }

                    let refs: BTreeSet<DataRef> = parsed.refs().into_iter().collect();
                    assert_eq!(
                        refs.len(),
                        names.len(),
                        "{case}: every key has its own page"
                    );

                    let reader = Reader::open(io).await.unwrap();
                    assert_eq!(reader.keys_count().await.unwrap(), names.len(), "{case}");
                    assert_reads_back(&reader, &names).await;
                }
            }
        }
    }
}

fn variable_entry(key: &[u8]) -> usize {
    (if key.len() < 128 { 1 } else { 2 }) + key.len() + 5
}

#[tokio::test]
async fn data_pages_roll_over_to_the_next_file_at_max_data_file_bytes() {
    let mut rng = seeded(12);
    let (rows, names) = rows(&keys_of(&[70, 10, 50, 30, 60, 20, 40]), &mut rng);

    for max in [2 * DATA_PAGE + 100, 2 * DATA_PAGE] {
        let io = InMemoryIo::default();
        let mut writer = Writer::new_with_capacity(io.clone(), 1024, 64)
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

        let refs: BTreeSet<DataRef> =
            parse_index(&file_bytes(&io, FileRole::Index).await, Some(64))
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
        assert_reads_back(&reader, &names).await;
    }
}

#[tokio::test]
async fn a_known_key_in_an_earlier_data_file_is_overwritten_in_place() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);
    writer.set_data_cache_size(0);

    write_keys(&mut writer, &keys_of(&[1, 2, 3])).await;
    assert_eq!(writer.data_files(), 3);

    let data_0 = io.file(FileRole::Data(0)).unwrap();
    assert_eq!((data_0.appends(), data_0.write_ats()), (1, 1));

    writer
        .write(Name(key(1)), Hour(5), &Record::with(15))
        .await
        .unwrap();
    writer
        .write(Name(key(1)), Hour(0), &Record::with(10))
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
        reader.get(&Name(key(1)), Hour(0), Hour(5)).await.unwrap(),
        Some(expected)
    );
}

#[tokio::test]
async fn the_257th_data_file_is_a_contract_violation_and_the_writer_stays_usable() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);

    for n in 0..256u64 {
        writer
            .write(Name(key(n)), Hour(1), &Record::with(n))
            .await
            .unwrap();
    }

    for _ in 0..2 {
        assert!(matches!(
            writer
                .write(Name(key(256)), Hour(1), &Record::with(256))
                .await,
            Err(ArchiveError::ContractViolation(_))
        ));
        assert_eq!(writer.keys_count(), 256);
        assert_eq!(writer.data_files(), 256);
    }

    writer
        .write(Name(key(7)), Hour(2), &Record::with(72))
        .await
        .unwrap();
    writer.finish().await.unwrap();
    assert_eq!(io.roles().len(), 257);

    let reader = Reader::open(io).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), 256);

    for n in 0..256u64 {
        assert_eq!(
            reader.get(&Name(key(n)), Hour(1), Hour(1)).await.unwrap(),
            Some(vec![Record::with(n)])
        );
    }
    assert_eq!(
        reader.get(&Name(key(7)), Hour(2), Hour(2)).await.unwrap(),
        Some(vec![Record::with(72)])
    );
    assert_eq!(
        reader.get(&Name(key(256)), Hour(1), Hour(1)).await.unwrap(),
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
        .write(Name(key(1)), Hour(0), &Record::with(1))
        .await
        .unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);
}

#[tokio::test]
async fn files_are_created_only_when_first_needed() {
    // A 52-byte page has 40 bytes for entries: two 15-byte entries, not three.
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), 64, 52).await.unwrap();
    writer.set_max_data_file_bytes(2 * DATA_PAGE as u64);
    writer.set_l1_cache_size(1);
    assert_eq!(io.created_roles(), [FileRole::Index]);

    write_keys(&mut writer, &keys_of(&[1, 2])).await;
    assert_eq!(io.created_roles(), [FileRole::Index, FileRole::Data(0)]);

    write_keys(&mut writer, &keys_of(&[3])).await;
    assert_eq!(
        io.created_roles(),
        [FileRole::Index, FileRole::Data(0), FileRole::Data(1)],
        "data file 1 comes with its first page"
    );

    let index = io.file(FileRole::Index).unwrap();
    assert_eq!(index.readers_opened(), 0, "every L1 page so far was cached");

    // Page [1, 2] was evicted by page [3]: reading it back opens the index's reader, once.
    writer
        .write(Name(key(1)), Hour(1), &Record::with(11))
        .await
        .unwrap();
    assert_eq!(index.readers_opened(), 1);

    writer
        .write(Name(key(3)), Hour(1), &Record::with(31))
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

    let mut rows = hour_0_rows(&keys_of(&[1, 2, 3]));
    rows.push((key(1), 1, Record::with(11)));
    rows.push((key(3), 1, Record::with(31)));
    assert_reads_back(&Reader::open(io).await.unwrap(), &names_of(&rows)).await;
}

#[tokio::test]
async fn finish_finishes_every_writer_even_when_one_fails() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone()).await.unwrap();
    writer.set_max_data_file_bytes(DATA_PAGE as u64);
    write_keys(&mut writer, &keys_of(&[1, 2, 3])).await;

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
        write_keys(&mut writer, &keys_of(&[1, 2, 3])).await;

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
        l0_frame(64 * 1024, 0, 0, &[])
    );

    let mut rng = seeded(18);
    let keys = random_keys(20, 30, &mut rng);
    let (rows, _) = rows(&keys, &mut rng);
    let io = build(&rows, 1032, 1024, 2, Some(4)).await;

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
