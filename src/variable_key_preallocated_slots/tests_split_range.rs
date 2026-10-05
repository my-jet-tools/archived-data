use super::split_test_support::{
    DATA_PAGE, Reader, Row, Writer, build, compact, data_read_ats, data_readers_opened, file_bytes,
    hand_built, hour_0_rows, key, numbered_keys, patch_u16, random_keys, rows, seeded,
};
use super::test_types::{Hour, Name, Names, Record};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::{ArchiveError, MAX_VARIABLE_KEY_LEN, last_key_starting_with};

type Range = Vec<(Name, Vec<Record>)>;

/// Sorts after every key of 40 bytes or fewer.
fn above_every_key() -> Vec<u8> {
    vec![0xFF; 64]
}

/// What `get_range` must answer, straight from the rows.
fn expected(names: &Names, from: &[u8], to: &[u8], from_slot: usize, to_slot: usize) -> Range {
    if from > to {
        return Vec::new();
    }

    names
        .range(from.to_vec()..=to.to_vec())
        .map(|(key, hours)| {
            let items = (from_slot..=to_slot)
                .map(|hour| hours[hour].clone().unwrap_or_default())
                .collect();
            (Name(key.clone()), items)
        })
        .collect()
}

async fn range(reader: &Reader, from: &[u8], to: &[u8], from_slot: usize, to_slot: usize) -> Range {
    reader
        .get_range(
            &Name(from.to_vec()),
            &Name(to.to_vec()),
            Hour(from_slot),
            Hour(to_slot),
        )
        .await
        .unwrap()
}

/// `count` numbered keys, with their compact archive under `pages_per_file` data pages per file.
async fn compact_names(
    count: u64,
    seed: u64,
    pages_per_file: Option<usize>,
) -> (InMemoryIo, Names) {
    let mut rng = seeded(seed);
    let (rows, names) = rows(&numbered_keys(count), &mut rng);

    let source = Reader::open(build(&rows, 64 * 1024, 16 * 1024, 64, None).await)
        .await
        .unwrap();
    if let Some(pages) = pages_per_file {
        source.set_max_data_file_bytes((pages * DATA_PAGE) as u64);
    }

    (compact(&source).await, names)
}

#[tokio::test]
async fn a_range_is_every_key_between_its_bounds_in_key_order() {
    let mut rng = seeded(4040);
    let keys = random_keys(300, 40, &mut rng);
    let (rows, names) = rows(&keys, &mut rng);

    // 128-byte pages hold two to ten keys, and three data pages per file: most ranges cross pages and
    // data files.
    let reader = Reader::open(build(&rows, 1024, 128, 2, Some(3)).await)
        .await
        .unwrap();
    assert!(reader.l0_intervals() > 30);

    let after = |key: &[u8]| [key, &[0]].concat();
    let last = above_every_key();

    let bounds: [(Vec<u8>, Vec<u8>); 10] = [
        (keys[10].clone(), keys[11].clone()),
        (keys[10].clone(), keys[10].clone()),
        (keys[40].clone(), keys[200].clone()),
        (after(&keys[40]), after(&keys[199])),
        (after(&keys[5]), after(&keys[5])),
        (Vec::new(), keys[0].clone()),
        (Vec::new(), Vec::new()),
        (keys[299].clone(), last.clone()),
        (after(&keys[299]), last.clone()),
        (Vec::new(), last),
    ];

    for (from, to) in bounds {
        for (from_slot, to_slot) in [(0, 23), (5, 9), (7, 7)] {
            assert!(
                range(&reader, &from, &to, from_slot, to_slot).await
                    == expected(&names, &from, &to, from_slot, to_slot),
                "{from:?}..={to:?}, slots {from_slot}..={to_slot}"
            );
        }
    }
}

/// The keys of `range` alone.
fn keys_of(range: &Range) -> Vec<Vec<u8>> {
    range.iter().map(|(key, _)| key.0.clone()).collect()
}

#[tokio::test]
async fn a_range_up_to_the_last_key_starting_with_a_prefix_is_every_key_that_starts_with_it() {
    let prefix = b"node/".to_vec();

    // Just outside the prefix on either side, the prefix itself, suffixes of every length, and suffixes
    // that start with 0xFF and go on past it.
    let mut keys: Vec<Vec<u8>> = [
        &b"node."[..],
        b"node/",
        b"node/a",
        b"node/ab",
        b"node/abcdefgh",
        b"node/\xFF\x01",
        b"node0",
    ]
    .iter()
    .map(|key| key.to_vec())
    .collect();
    keys.extend([
        [&prefix[..], &[0xFF; 6], &[0x01]].concat(),
        [&prefix[..], &[0xFF; 40]].concat(),
        [&prefix[..], &[0x00; 30]].concat(),
    ]);

    let mut rng = seeded(0xAB);
    keys.extend(random_keys(200, 12, &mut rng));
    for suffix in random_keys(150, 40, &mut rng) {
        keys.push([&prefix[..], &suffix].concat());
    }
    keys.sort();
    keys.dedup();

    let (rows, names) = rows(&keys, &mut rng);
    let reader = Reader::open(build(&rows, 1024, 128, 4, Some(7)).await)
        .await
        .unwrap();

    let starting: Vec<Vec<u8>> = keys
        .iter()
        .filter(|key| key.starts_with(&prefix))
        .cloned()
        .collect();
    assert!(starting.len() > 150);

    let last = last_key_starting_with(&prefix).unwrap();
    assert_eq!(last.len(), MAX_VARIABLE_KEY_LEN);

    let got = range(&reader, &prefix, &last, 0, 23).await;
    assert_eq!(keys_of(&got), starting);
    assert!(got == expected(&names, &prefix, &last, 0, 23));

    // A short run of 0xFF is not that bound. With one, the range holds keys far longer than one byte past
    // the prefix, and misses a key whose suffix starts with 0xFF and goes on.
    let one = range(&reader, &prefix, &[&prefix[..], &[0xFF]].concat(), 0, 0).await;
    let one = keys_of(&one);
    assert!(one.contains(&b"node/abcdefgh".to_vec()));
    assert!(!one.contains(&b"node/\xFF\x01".to_vec()));

    // With six, it misses the key that has seven.
    let six = range(&reader, &prefix, &[&prefix[..], &[0xFF; 6]].concat(), 0, 0).await;
    assert!(!keys_of(&six).contains(&[&prefix[..], &[0xFF; 6], &[0x01]].concat()));
    assert!(keys_of(&six).len() < starting.len());
}

#[tokio::test]
async fn the_whole_archive_is_get_of_every_key_and_fills_no_data_cache() {
    let mut rng = seeded(77);
    let keys = random_keys(500, 40, &mut rng);
    let (rows, _) = rows(&keys, &mut rng);

    let io = build(&rows, 1024, 256, 3, Some(40)).await;
    let reader = Reader::open(io).await.unwrap();

    let whole = range(&reader, &[], &above_every_key(), 0, 23).await;
    assert_eq!(whole.len(), keys.len());
    assert!(reader.data_cache.pages.read().await.is_empty());

    for (key, items) in whole {
        assert!(
            reader.get(&key, Hour(0), Hour(23)).await.unwrap() == Some(items),
            "{key:?}"
        );
    }
}

#[tokio::test]
async fn from_after_to_reads_nothing_and_from_slot_after_to_slot_reads_no_data_page() {
    let (io, names) = compact_names(1_500, 3, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let index = io.file(FileRole::Index).unwrap();
    let opened = index.read_ats();

    let keys: Vec<Vec<u8>> = names.keys().cloned().collect();

    assert!(range(&reader, &keys[9], &keys[3], 0, 23).await.is_empty());
    assert!(
        range(&reader, &above_every_key(), &[], 9, 3)
            .await
            .is_empty()
    );
    assert_eq!(index.read_ats(), opened, "nothing was read");
    assert!(reader.l1_cache.pages.read().await.is_empty());

    let keys_only = range(&reader, &keys[900], &keys[930], 9, 3).await;
    let expected: Range = keys[900..=930]
        .iter()
        .map(|key| (Name(key.clone()), Vec::new()))
        .collect();
    assert!(keys_only == expected);

    assert_eq!(index.read_ats(), opened + 1, "one L1 page");
    assert_eq!((data_readers_opened(&io), data_read_ats(&io)), (0, 0));
}

#[tokio::test]
async fn an_empty_archive_or_a_range_between_keys_is_empty() {
    let empty = InMemoryIo::default();
    Writer::new(empty.clone())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
    let reader = Reader::open(empty.clone()).await.unwrap();

    assert!(
        range(&reader, &[], &above_every_key(), 0, 23)
            .await
            .is_empty()
    );
    assert_eq!(empty.roles(), [FileRole::Index]);

    let rows: Vec<Row> = hour_0_rows(&[key(10), key(20), key(30)]);
    let io = build(&rows, 1024, 64, 2, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();

    for (from, to) in [
        (key(11), key(19)),
        (key(21), key(29)),
        (Vec::new(), key(9)),
        (key(31), above_every_key()),
    ] {
        assert!(range(&reader, &from, &to, 0, 23).await.is_empty());
    }
    assert_eq!(data_readers_opened(&io), 0);
}

#[tokio::test]
async fn a_contiguous_range_of_a_compact_archive_is_a_handful_of_reads() {
    // 3,000 nine-byte keys: three compact L1 pages of 1024. (pages per data file, keys asked for, data
    // reads expected)
    let cases = [
        // One data file of 9.2 MiB, read in runs of at most 8 MiB: 2,721 pages, then the rest.
        (None, 0..3_000, 2),
        (None, 1_200..1_700, 1),
        // A thousand pages per file: one read per file.
        (Some(1_000), 0..3_000, 3),
        (Some(1_000), 900..1_100, 2),
    ];

    for (pages_per_file, keys, reads) in cases {
        let case = format!("{pages_per_file:?} pages per file, keys {keys:?}");
        let (io, names) = compact_names(3_000, 12, pages_per_file).await;
        let all: Vec<Vec<u8>> = names.keys().cloned().collect();
        assert_eq!(all.len(), 3_000);

        let reader = Reader::open(io.clone()).await.unwrap();
        let index = io.file(FileRole::Index).unwrap();
        let opened = index.read_ats();

        let (from, to) = (&all[keys.start], &all[keys.end - 1]);
        assert!(
            range(&reader, from, to, 0, 23).await == expected(&names, from, to, 0, 23),
            "{case}"
        );

        // The L1 pages of the range lie back to back in the index: one read, however many there are.
        assert_eq!(index.read_ats() - opened, 1, "{case}");
        assert_eq!(data_read_ats(&io), reads, "{case}");
        assert_eq!(io.file(FileRole::Data(0)).unwrap().eof_reads(), 0, "{case}");
    }
}

/// The keys of every L1 page cached by `reader`, as offsets, ascending.
async fn cached_l1_offsets(reader: &Reader) -> Vec<u64> {
    let mut offsets: Vec<u64> = reader.l1_cache.pages.read().await.keys().copied().collect();
    offsets.sort_unstable();
    offsets
}

#[tokio::test]
async fn a_wide_range_reads_its_l1_pages_in_one_run_and_again_only_what_the_cache_lost() {
    // 12,000 nine-byte keys: twelve compact L1 pages back to back, one data file of 37 MiB.
    let (io, names) = compact_names(12_000, 21, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    assert_eq!(reader.l0_intervals(), 12);

    let index = io.file(FileRole::Index).unwrap();
    let opened = index.read_ats();
    let last = above_every_key();
    let whole = expected(&names, &[], &last, 0, 23);

    assert!(range(&reader, &[], &last, 0, 23).await == whole);
    assert_eq!(index.read_ats() - opened, 1, "twelve pages, one read");
    assert_eq!(data_read_ats(&io), 5, "37 MiB in runs of at most 8 MiB");

    // Every page was cached on the way; a cache of 10 kept the last ten.
    let pages: Vec<u64> = reader
        .l0
        .iter()
        .map(|interval| interval.l1_offset as u64)
        .collect();
    assert_eq!(cached_l1_offsets(&reader).await, pages[2..]);

    reader.set_l1_cache_size(64).await;
    assert!(range(&reader, &[], &last, 0, 23).await == whole);
    assert_eq!(
        index.read_ats() - opened,
        2,
        "the two pages evicted, in one read"
    );
    assert_eq!(cached_l1_offsets(&reader).await, pages);

    // Warm: the same range, and any range inside it, reads no index byte.
    let keys: Vec<Vec<u8>> = names.keys().cloned().collect();
    assert!(range(&reader, &[], &last, 3, 4).await == expected(&names, &[], &last, 3, 4));
    assert!(
        range(&reader, &keys[1_500], &keys[9_000], 0, 23).await
            == expected(&names, &keys[1_500], &keys[9_000], 0, 23)
    );
    for key in [&keys[0], &keys[5_000], &keys[11_999]] {
        reader
            .get(&Name(key.clone()), Hour(0), Hour(0))
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(index.read_ats() - opened, 2, "a warm cache reads nothing");
}

#[tokio::test]
async fn a_working_index_whose_pages_are_scattered_answers_every_range_and_warms_the_cache() {
    let mut rng = seeded(606);
    let keys = random_keys(600, 20, &mut rng);
    let (rows, names) = rows(&keys, &mut rng);

    // 96-byte pages, written in no order: pages split, and L0 order is not file order.
    let io = build(&rows, 1024, 96, 8, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let pages = reader.l0.len();
    assert!(pages > 100);
    assert!(
        reader
            .l0
            .windows(2)
            .any(|pair| pair[1].l1_offset < pair[0].l1_offset),
        "the test needs pages out of file order"
    );

    reader.set_l1_cache_size(pages).await;
    let index = io.file(FileRole::Index).unwrap();
    let opened = index.read_ats();

    let last = above_every_key();
    let whole = range(&reader, &[], &last, 0, 23).await;
    assert!(whole == expected(&names, &[], &last, 0, 23));

    let cold = index.read_ats() - opened;
    assert!(
        (1..=pages).contains(&cold),
        "{cold} reads for {pages} pages"
    );
    assert_eq!(cached_l1_offsets(&reader).await.len(), pages);

    for (key, items) in &whole {
        assert!(
            reader.get(key, Hour(0), Hour(23)).await.unwrap().as_ref() == Some(items),
            "{key:?}"
        );
    }

    assert!(
        range(&reader, &keys[100], &keys[500], 1, 2).await
            == expected(&names, &keys[100], &keys[500], 1, 2)
    );
    assert_eq!(
        index.read_ats() - opened,
        cold,
        "a warm cache reads nothing"
    );
}

#[tokio::test]
async fn a_working_archive_written_in_any_order_reads_a_dense_range_in_runs() {
    let mut rng = seeded(4242);
    let keys = numbered_keys(6_000);
    let (rows, names) = rows(&keys, &mut rng);
    let last = above_every_key();

    // (data pages per file, data reads expected for the whole range): one run per file, 8 MiB at most —
    // 2,721 pages of 3,082 B.
    for (pages_per_file, reads) in [(None, 3), (Some(2_000), 3), (Some(1_000), 6)] {
        let case = format!("{pages_per_file:?} pages per file");
        let io = build(&rows, 64 * 1024, 16 * 1024, 64, pages_per_file).await;
        let reader = Reader::open(io.clone()).await.unwrap();

        // Rows arrive shuffled, so key order and file order disagree almost everywhere.
        let mut refs = Vec::new();
        for key in &keys {
            refs.push(
                reader
                    .data_ref_of(&Name(key.clone()))
                    .await
                    .unwrap()
                    .unwrap()
                    .cache_key(),
            );
        }
        assert!(
            refs.windows(2).filter(|pair| pair[1] < pair[0]).count() > 2_000,
            "{case}: the test needs keys out of file order"
        );

        let reads_before = data_read_ats(&io);
        let whole = range(&reader, &[], &last, 0, 23).await;
        assert_eq!(data_read_ats(&io) - reads_before, reads, "{case}");
        assert!(whole == expected(&names, &[], &last, 0, 23), "{case}");

        for (key, items) in &whole {
            assert!(
                reader.get(key, Hour(0), Hour(23)).await.unwrap().as_ref() == Some(items),
                "{case}: {key:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_working_archive_answers_every_range_as_its_compaction() {
    let mut rng = seeded(909);
    let keys = random_keys(400, 40, &mut rng);
    let (rows, names) = rows(&keys, &mut rng);

    let working = Reader::open(build(&rows, 256, 128, 2, Some(5)).await)
        .await
        .unwrap();
    assert!(working.l0_frames() > 1, "the test must chain");
    working.set_max_data_file_bytes(60 * DATA_PAGE as u64);
    let compacted = Reader::open(compact(&working).await).await.unwrap();

    for (from, to) in [
        (Vec::new(), above_every_key()),
        (keys[0].clone(), keys[99].clone()),
        ([&keys[150][..], &[0]].concat(), keys[390].clone()),
        (keys[399].clone(), keys[399].clone()),
    ] {
        let from_working = range(&working, &from, &to, 2, 20).await;
        assert!(from_working == range(&compacted, &from, &to, 2, 20).await);
        assert!(from_working == expected(&names, &from, &to, 2, 20));
    }
}

/// Every slot of the keys from `from` to `to`, `Err` included.
async fn whole(reader: &Reader, from: &[u8], to: &[u8]) -> Result<Range, ArchiveError> {
    reader
        .get_range(&Name(from.to_vec()), &Name(to.to_vec()), Hour(0), Hour(23))
        .await
}

#[tokio::test]
async fn a_damaged_or_unreadable_data_page_inside_a_run_is_an_error_never_a_partial_answer() {
    let (io, names) = compact_names(100, 5, None).await;
    let keys: Vec<Vec<u8>> = names.keys().cloned().collect();
    let data = file_bytes(&io, FileRole::Data(0)).await;
    let index = file_bytes(&io, FileRole::Index).await;

    let damaged = |bytes: Vec<u8>| {
        let io = InMemoryIo::default();
        io.put(FileRole::Index, index.clone());
        io.put(FileRole::Data(0), bytes);
        io
    };

    let mut item_len = data.clone();
    patch_u16(&mut item_len, 50 * DATA_PAGE + 8, 127);
    let cut = data[..60 * DATA_PAGE + 100].to_vec();

    for (case, io) in [
        ("a wrong item_len", damaged(item_len)),
        ("a cut data file", damaged(cut)),
    ] {
        let reader = Reader::open(io.clone()).await.unwrap();

        assert!(
            matches!(
                whole(&reader, &keys[10], &keys[90]).await,
                Err(ArchiveError::Corrupted(_))
            ),
            "{case}"
        );
        assert!(
            range(&reader, &keys[10], &keys[40], 0, 23).await
                == expected(&names, &keys[10], &keys[40], 0, 23),
            "{case}: a range before the damage"
        );
        assert_eq!(io.file(FileRole::Data(0)).unwrap().eof_reads(), 0, "{case}");
    }

    let reader = Reader::open(io.clone()).await.unwrap();
    io.file(FileRole::Data(0)).unwrap().fail_reads();
    assert!(matches!(
        whole(&reader, &[], &above_every_key()).await,
        Err(ArchiveError::Io(_))
    ));

    let missing = InMemoryIo::default();
    missing.put(FileRole::Index, index);
    let reader = Reader::open(missing).await.unwrap();
    assert!(matches!(
        whole(&reader, &[], &above_every_key()).await,
        Err(ArchiveError::Io(_))
    ));
}

#[tokio::test]
async fn chained_frames_read_back_as_get_reads_them() {
    let archive = hand_built();
    let io = archive.io();
    let reader = Reader::open(io.clone()).await.unwrap();

    let last = above_every_key();
    assert!(range(&reader, &[], &last, 0, 23).await == expected(&archive.names, &[], &last, 0, 23));
    assert!(range(&reader, b"b", b"c", 6, 8).await == expected(&archive.names, b"b", b"c", 6, 8));

    for role in io.roles() {
        assert_eq!(io.file(role).unwrap().eof_reads(), 0, "{role:?}");
    }
}

#[tokio::test]
async fn a_slot_out_of_range_is_refused_before_anything_else() {
    let (io, _) = compact_names(10, 1, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let index = io.file(FileRole::Index).unwrap();
    let opened = index.read_ats();

    for (from, to, from_slot, to_slot) in [
        (Vec::new(), above_every_key(), 0, 24),
        (key(9), key(1), 24, 0),
    ] {
        assert!(matches!(
            reader
                .get_range(&Name(from), &Name(to), Hour(from_slot), Hour(to_slot))
                .await,
            Err(ArchiveError::ContractViolation(_))
        ));
    }

    assert_eq!(index.read_ats(), opened, "nothing was read");
    assert_eq!(data_readers_opened(&io), 0);
}
