use super::split_test_support::{
    DATA_PAGE, Day, Reader, Row, Writer, build, compact, data_read_ats, data_readers_opened,
    file_bytes, hand_built, logins, patch_u16, rows, seeded,
};
use super::test_types::{Hour, Login, Record, Rng};
use crate::ArchiveError;
use crate::in_memory_io::{FileRole, InMemoryIo};

type Range = Vec<(Login, Vec<Record>)>;

/// What `get_range` must answer, straight from the rows.
fn expected(day: &Day, from: u64, to: u64, from_slot: usize, to_slot: usize) -> Range {
    if from > to {
        return Vec::new();
    }

    day.range(from..=to)
        .map(|(login, hours)| {
            let items = (from_slot..=to_slot)
                .map(|hour| hours[hour].clone().unwrap_or_default())
                .collect();
            (Login(*login), items)
        })
        .collect()
}

async fn range(reader: &Reader, from: u64, to: u64, from_slot: usize, to_slot: usize) -> Range {
    reader
        .get_range(&Login(from), &Login(to), Hour(from_slot), Hour(to_slot))
        .await
        .unwrap()
}

/// A day of `count` logins, with its compact archive under `pages_per_file` data pages per file.
async fn compact_day(count: u64, seed: u64, pages_per_file: Option<usize>) -> (InMemoryIo, Day) {
    let mut rng = seeded(seed);
    let logins = logins(count, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    let source = Reader::open(build(&rows, 1024, 1024, 64, None).await)
        .await
        .unwrap();
    if let Some(pages) = pages_per_file {
        source.set_max_data_file_bytes((pages * DATA_PAGE) as u64);
    }

    (compact(&source).await, day)
}

#[tokio::test]
async fn a_range_is_every_key_between_its_bounds_in_key_order() {
    let mut rng = Rng(4040);
    let logins = logins(300, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    // Four keys per L1 page and three data pages per file: most ranges cross pages and data files.
    let reader = Reader::open(build(&rows, 1024, 4, 2, Some(3)).await)
        .await
        .unwrap();
    assert!(reader.l0_intervals() > 50);

    let first = logins[0];
    let last = logins[logins.len() - 1];

    let bounds = [
        (logins[10], logins[11]),
        (logins[10], logins[10]),
        (logins[40], logins[200]),
        (logins[40] + 1, logins[200] - 1),
        (logins[5] - 1, logins[5] - 1),
        (0, first),
        (0, first - 1),
        (last, u64::MAX),
        (last + 1, u64::MAX),
        (0, u64::MAX),
    ];

    for (from, to) in bounds {
        for (from_slot, to_slot) in [(0, 23), (5, 9), (7, 7)] {
            assert_eq!(
                range(&reader, from, to, from_slot, to_slot).await,
                expected(&day, from, to, from_slot, to_slot),
                "{from}..={to}, slots {from_slot}..={to_slot}"
            );
        }
    }
}

#[tokio::test]
async fn the_whole_archive_is_get_of_every_key_and_fills_no_data_cache() {
    let mut rng = Rng(77);
    let logins = logins(500, &mut rng);
    let (rows, _) = rows(&logins, &mut rng);

    let io = build(&rows, 16, 8, 3, Some(40)).await;
    let reader = Reader::open(io).await.unwrap();

    let whole = range(&reader, 0, u64::MAX, 0, 23).await;
    assert_eq!(whole.len(), logins.len());
    assert!(reader.data_cache.pages.read().await.is_empty());

    for (login, items) in whole {
        assert_eq!(
            reader.get(&login, Hour(0), Hour(23)).await.unwrap(),
            Some(items),
            "{login:?}"
        );
    }
}

#[tokio::test]
async fn from_after_to_reads_nothing_and_from_slot_after_to_slot_reads_no_data_page() {
    let (io, day) = compact_day(1_500, 3, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let index = io.file(FileRole::Index).unwrap();
    let opened = index.read_ats();

    let logins: Vec<u64> = day.keys().copied().collect();

    assert_eq!(
        range(&reader, logins[9], logins[3], 0, 23).await,
        Vec::new()
    );
    assert_eq!(range(&reader, u64::MAX, 0, 9, 3).await, Vec::new());
    assert_eq!(index.read_ats(), opened, "nothing was read");
    assert!(reader.l1_cache.pages.read().await.is_empty());

    let keys_only = range(&reader, logins[900], logins[930], 9, 3).await;
    let expected: Range = logins[900..=930]
        .iter()
        .map(|login| (Login(*login), Vec::new()))
        .collect();
    assert_eq!(keys_only, expected);

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

    assert_eq!(range(&reader, 0, u64::MAX, 0, 23).await, Vec::new());
    assert_eq!(empty.roles(), [FileRole::Index]);

    let rows: Vec<Row> = [10, 20, 30]
        .into_iter()
        .map(|login| (login, 0, Record::with(login)))
        .collect();
    let io = build(&rows, 16, 2, 2, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();

    for (from, to) in [(11, 19), (21, 29), (0, 9), (31, u64::MAX)] {
        assert_eq!(range(&reader, from, to, 0, 23).await, Vec::new());
    }
    assert_eq!(data_readers_opened(&io), 0);
}

#[tokio::test]
async fn a_contiguous_range_of_a_compact_archive_is_a_handful_of_reads() {
    // 3,000 keys: three compact L1 pages. (pages per data file, keys asked for, data reads expected)
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
        let (io, day) = compact_day(3_000, 12, pages_per_file).await;
        let logins: Vec<u64> = day.keys().copied().collect();
        assert_eq!(logins.len(), 3_000);

        let reader = Reader::open(io.clone()).await.unwrap();
        let index = io.file(FileRole::Index).unwrap();
        let opened = index.read_ats();

        let (from, to) = (logins[keys.start], logins[keys.end - 1]);
        assert_eq!(
            range(&reader, from, to, 0, 23).await,
            expected(&day, from, to, 0, 23),
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
    // 12,000 keys: twelve compact L1 pages back to back, one data file of 37 MiB.
    let (io, day) = compact_day(12_000, 21, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    assert_eq!(reader.l0_intervals(), 12);

    let index = io.file(FileRole::Index).unwrap();
    let opened = index.read_ats();
    let whole = expected(&day, 0, u64::MAX, 0, 23);

    assert_eq!(range(&reader, 0, u64::MAX, 0, 23).await, whole);
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
    assert_eq!(range(&reader, 0, u64::MAX, 0, 23).await, whole);
    assert_eq!(
        index.read_ats() - opened,
        2,
        "the two pages evicted, in one read"
    );
    assert_eq!(cached_l1_offsets(&reader).await, pages);

    // Warm: the same range, and any range inside it, reads no index byte.
    let logins: Vec<u64> = day.keys().copied().collect();
    assert_eq!(
        range(&reader, 0, u64::MAX, 3, 4).await,
        expected(&day, 0, u64::MAX, 3, 4)
    );
    assert_eq!(
        range(&reader, logins[1_500], logins[9_000], 0, 23).await,
        expected(&day, logins[1_500], logins[9_000], 0, 23)
    );
    for login in [logins[0], logins[5_000], logins[11_999]] {
        reader
            .get(&Login(login), Hour(0), Hour(0))
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(index.read_ats() - opened, 2, "a warm cache reads nothing");
}

#[tokio::test]
async fn a_working_index_whose_pages_are_scattered_answers_every_range_and_warms_the_cache() {
    let mut rng = seeded(606);
    let logins = logins(600, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    // Four keys a page, written in no order: pages split, and L0 order is not file order.
    let io = build(&rows, 16, 4, 8, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let pages = reader.l0.len();
    assert!(pages > 150);
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

    let whole = range(&reader, 0, u64::MAX, 0, 23).await;
    assert_eq!(whole, expected(&day, 0, u64::MAX, 0, 23));

    let cold = index.read_ats() - opened;
    assert!(
        (1..=pages).contains(&cold),
        "{cold} reads for {pages} pages"
    );
    assert_eq!(cached_l1_offsets(&reader).await.len(), pages);

    for (login, items) in &whole {
        assert_eq!(
            reader.get(login, Hour(0), Hour(23)).await.unwrap().as_ref(),
            Some(items),
            "{login:?}"
        );
    }

    assert_eq!(
        range(&reader, logins[100], logins[500], 1, 2).await,
        expected(&day, logins[100], logins[500], 1, 2)
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
    let logins = logins(6_000, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    // (data pages per file, data reads expected for the whole range): one run per file, 8 MiB at most —
    // 2,721 pages of 3,082 B.
    for (pages_per_file, reads) in [(None, 3), (Some(2_000), 3), (Some(1_000), 6)] {
        let case = format!("{pages_per_file:?} pages per file");
        let io = build(&rows, 1024, 1024, 64, pages_per_file).await;
        let reader = Reader::open(io.clone()).await.unwrap();

        // Rows arrive shuffled, so key order and file order disagree almost everywhere.
        let refs: Vec<u64> = {
            let mut refs = Vec::new();
            for login in &logins {
                refs.push(
                    reader
                        .data_ref_of(&Login(*login))
                        .await
                        .unwrap()
                        .unwrap()
                        .cache_key(),
                );
            }
            refs
        };
        assert!(
            refs.windows(2).filter(|pair| pair[1] < pair[0]).count() > 2_000,
            "{case}: the test needs keys out of file order"
        );

        let reads_before = data_read_ats(&io);
        let whole = range(&reader, 0, u64::MAX, 0, 23).await;
        assert_eq!(data_read_ats(&io) - reads_before, reads, "{case}");
        assert_eq!(whole, expected(&day, 0, u64::MAX, 0, 23), "{case}");

        for (login, items) in &whole {
            assert_eq!(
                reader.get(login, Hour(0), Hour(23)).await.unwrap().as_ref(),
                Some(items),
                "{case}: {login:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_working_archive_answers_every_range_as_its_compaction() {
    let mut rng = seeded(909);
    let logins = logins(400, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    let working = Reader::open(build(&rows, 3, 7, 2, Some(5)).await)
        .await
        .unwrap();
    working.set_max_data_file_bytes(60 * DATA_PAGE as u64);
    let compacted = Reader::open(compact(&working).await).await.unwrap();

    for (from, to) in [
        (0, u64::MAX),
        (logins[0], logins[99]),
        (logins[150] + 1, logins[390]),
        (logins[399], logins[399]),
    ] {
        let from_working = range(&working, from, to, 2, 20).await;
        assert_eq!(from_working, range(&compacted, from, to, 2, 20).await);
        assert_eq!(from_working, expected(&day, from, to, 2, 20));
    }
}

#[tokio::test]
async fn a_damaged_or_unreadable_data_page_inside_a_run_is_an_error_never_a_partial_answer() {
    let (io, day) = compact_day(100, 5, None).await;
    let logins: Vec<u64> = day.keys().copied().collect();
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
                reader
                    .get_range(&Login(logins[10]), &Login(logins[90]), Hour(0), Hour(23))
                    .await,
                Err(ArchiveError::Corrupted(_))
            ),
            "{case}"
        );
        assert_eq!(
            range(&reader, logins[10], logins[40], 0, 23).await,
            expected(&day, logins[10], logins[40], 0, 23),
            "{case}: a range before the damage"
        );
        assert_eq!(io.file(FileRole::Data(0)).unwrap().eof_reads(), 0, "{case}");
    }

    let reader = Reader::open(io.clone()).await.unwrap();
    io.file(FileRole::Data(0)).unwrap().fail_reads();
    assert!(matches!(
        reader
            .get_range(&Login(0), &Login(u64::MAX), Hour(0), Hour(0))
            .await,
        Err(ArchiveError::Io(_))
    ));

    let missing = InMemoryIo::default();
    missing.put(FileRole::Index, index);
    let reader = Reader::open(missing).await.unwrap();
    assert!(matches!(
        reader
            .get_range(&Login(0), &Login(u64::MAX), Hour(0), Hour(0))
            .await,
        Err(ArchiveError::Io(_))
    ));
}

#[tokio::test]
async fn chained_frames_read_back_as_get_reads_them() {
    let archive = hand_built();
    let io = archive.io();
    let reader = Reader::open(io.clone()).await.unwrap();

    assert_eq!(
        range(&reader, 0, u64::MAX, 0, 23).await,
        expected(&archive.day, 0, u64::MAX, 0, 23)
    );
    assert_eq!(
        range(&reader, 2, 3, 6, 8).await,
        expected(&archive.day, 2, 3, 6, 8)
    );

    for role in io.roles() {
        assert_eq!(io.file(role).unwrap().eof_reads(), 0, "{role:?}");
    }
}

#[tokio::test]
async fn a_slot_out_of_range_is_refused_before_anything_else() {
    let (io, _) = compact_day(10, 1, None).await;
    let reader = Reader::open(io.clone()).await.unwrap();
    let index = io.file(FileRole::Index).unwrap();
    let opened = index.read_ats();

    for (from, to, from_slot, to_slot) in [(0, u64::MAX, 0, 24), (9, 1, 24, 0)] {
        assert!(matches!(
            reader
                .get_range(&Login(from), &Login(to), Hour(from_slot), Hour(to_slot))
                .await,
            Err(ArchiveError::ContractViolation(_))
        ));
    }

    assert_eq!(index.read_ats(), opened, "nothing was read");
    assert_eq!(data_readers_opened(&io), 0);
}
