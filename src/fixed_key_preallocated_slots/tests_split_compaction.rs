use super::split_test_support::{
    DATA_PAGE, DEFAULT_PAGES_PER_FILE, Day, Reader, Row, Writer, assert_reads_back, build, compact,
    data_files, file_bytes, hand_built, l0_frame_len, l1_page_len, le_u32_at, logins, parse_index,
    reference_split_day, rows, wait_until,
};
use super::test_types::{Hour, Login, Record, Rng};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::{ArchiveError, CompactionPlan, DEFAULT_MAX_DATA_FILE_BYTES, l0_frame};

#[tokio::test]
async fn compact_output_is_byte_identical_to_the_reference_whatever_the_write_order() {
    let mut rng = Rng(0xDEAD_BEEF);
    // Past two compact L1 pages (1024), with a partial last page.
    let logins = logins(2_500, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    let mut reversed = rows.clone();
    reversed.reverse();

    // (rows, capacity_l1, cache, data pages per file of the source)
    let sources = [
        (&rows, 1024, 64, None),
        (&reversed, 16, 3, Some(12)),
        (&rows, 7, 1, Some(40)),
    ];

    for (rows, capacity_l1, cache, source_pages) in sources {
        let source = Reader::open(build(rows, 1024, capacity_l1, cache, source_pages).await)
            .await
            .unwrap();

        let mut out = InMemoryIo::default();

        for pages_per_file in [None, Some(10), Some(64)] {
            let case =
                format!("capacity_l1 {capacity_l1}, compact pages per file {pages_per_file:?}");

            source.set_max_data_file_bytes(
                pages_per_file.map_or(DEFAULT_MAX_DATA_FILE_BYTES, |pages| {
                    (pages * DATA_PAGE) as u64
                }),
            );

            let (index, data) =
                reference_split_day(&day, pages_per_file.unwrap_or(DEFAULT_PAGES_PER_FILE));

            out = compact(&source).await;
            assert!(
                file_bytes(&out, FileRole::Index).await == index,
                "index differs: {case}"
            );
            assert!(data_files(&out).await == data, "data differs: {case}");
        }

        // A compact archive is itself a readable archive, and compacts to itself.
        let compacted = Reader::open(out.clone()).await.unwrap();
        compacted.set_max_data_file_bytes(64 * DATA_PAGE as u64);

        assert_eq!(compacted.l0_intervals(), 3);
        assert_eq!(compacted.l0_frames(), 1);
        assert_eq!(compacted.keys_count().await.unwrap(), day.len());
        assert_reads_back(&compacted, &day).await;

        let again = compact(&compacted).await;
        assert!(
            file_bytes(&again, FileRole::Index).await == file_bytes(&out, FileRole::Index).await
        );
        assert!(data_files(&again).await == data_files(&out).await);
    }
}

#[tokio::test]
async fn the_plan_sizes_are_exactly_what_is_written() {
    let mut rng = Rng(21);
    let logins = logins(300, &mut rng);
    let (rows, _) = rows(&logins, &mut rng);

    let reader = Reader::open(build(&rows, 1024, 16, 8, Some(7)).await)
        .await
        .unwrap();
    reader.set_max_data_file_bytes(40 * DATA_PAGE as u64 + 17);
    let plan = reader.compaction_plan().await.unwrap();

    assert_eq!(plan.keys_count(), 300);
    assert_eq!(
        plan.index_size(),
        (l0_frame_len(1) + l1_page_len(1024)) as u64
    );

    let mut sizes = vec![40 * DATA_PAGE as u64; 7];
    sizes.push(20 * DATA_PAGE as u64);
    assert_eq!(plan.data_file_sizes(), sizes);

    let out = InMemoryIo::default();
    reader.write_compacted_to(&plan, &out).await.unwrap();

    let mut roles: Vec<FileRole> = (0..8).map(FileRole::Data).collect();
    roles.push(FileRole::Index);
    assert_eq!(
        out.created_roles(),
        roles,
        "each data file in order, then the index"
    );

    assert_eq!(
        file_bytes(&out, FileRole::Index).await.len() as u64,
        plan.index_size()
    );
    for (no, size) in plan.data_file_sizes().iter().enumerate() {
        let file = file_bytes(&out, FileRole::Data(no as u8)).await;
        assert_eq!(file.len() as u64, *size, "data file {no}");
    }

    for role in out.roles() {
        let file = out.file(role).unwrap();
        assert_eq!(
            (file.write_ats(), file.finish_attempts(), file.finishes()),
            (0, 1, 1),
            "{role:?} is appended only and finished once"
        );
    }
}

#[tokio::test]
async fn an_empty_archive_plans_twenty_index_bytes_and_no_data_files() {
    let io = InMemoryIo::default();
    Writer::new(io.clone())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
    let reader = Reader::open(io).await.unwrap();

    let plan = reader.compaction_plan().await.unwrap();
    assert_eq!((plan.index_size(), plan.keys_count()), (20, 0));
    assert!(plan.data_file_sizes().is_empty());

    let out = InMemoryIo::default();
    reader.write_compacted_to(&plan, &out).await.unwrap();

    assert_eq!(out.created_roles(), [FileRole::Index]);
    assert_eq!(
        file_bytes(&out, FileRole::Index).await,
        l0_frame(20, 0, 0, &[])
    );
    assert_eq!(
        reference_split_day(&Day::new(), DEFAULT_PAGES_PER_FILE),
        (l0_frame(20, 0, 0, &[]), Vec::new())
    );
    assert_eq!(out.file(FileRole::Index).unwrap().finishes(), 1);

    let compacted = Reader::open(out).await.unwrap();
    assert_eq!(
        compacted.get(&Login(1), Hour(0), Hour(0)).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn a_plan_needing_257_data_files_is_a_contract_violation() {
    let rows: Vec<Row> = (0..257)
        .map(|login| (login, 0, Record::with(login)))
        .collect();
    let reader = Reader::open(build(&rows, 1024, 1024, 64, Some(2)).await)
        .await
        .unwrap();

    assert_eq!(reader.max_data_file_bytes(), DEFAULT_MAX_DATA_FILE_BYTES);
    assert_eq!(
        reader
            .compaction_plan()
            .await
            .unwrap()
            .data_file_sizes()
            .len(),
        1
    );

    reader.set_max_data_file_bytes(DATA_PAGE as u64);
    assert!(matches!(
        reader.compaction_plan().await,
        Err(ArchiveError::ContractViolation(_))
    ));

    reader.set_max_data_file_bytes(2 * DATA_PAGE as u64);
    assert_eq!(
        reader
            .compaction_plan()
            .await
            .unwrap()
            .data_file_sizes()
            .len(),
        129
    );
}

#[tokio::test]
async fn a_source_that_disagrees_with_the_plan_is_corrupted() {
    let archive = |logins: Vec<u64>| async move {
        let rows: Vec<Row> = logins
            .iter()
            .map(|login| (*login, 1, Record::with(*login)))
            .collect();
        Reader::open(build(&rows, 1024, 4, 8, None).await)
            .await
            .unwrap()
    };

    let planned = archive((1..=10).collect()).await;
    let plan = planned.compaction_plan().await.unwrap();

    let more = archive((1..=11).collect()).await;
    let fewer = archive((1..=9).collect()).await;
    let other_last = archive((1..=9).chain([12]).collect()).await;

    // The first two differ in the data pass and leave no index. The third writes every data file as
    // planned and differs only on its last L1 page: the index pass stops there, and the index its dropped
    // writer leaves behind is refused at open.
    for (case, source, index_left) in [
        ("one key more", &more, false),
        ("one key fewer", &fewer, false),
        ("the same count, another last key", &other_last, true),
    ] {
        let out = InMemoryIo::default();

        assert!(
            matches!(
                source.write_compacted_to(&plan, &out).await,
                Err(ArchiveError::Corrupted(_))
            ),
            "{case}"
        );

        assert_eq!(out.file(FileRole::Index).is_some(), index_left, "{case}");
        match Reader::open(out).await {
            Err(ArchiveError::Io(err)) if !index_left => {
                assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{case}")
            }
            Err(ArchiveError::Corrupted(_)) if index_left => {}
            other => panic!("{case}: {:?}", other.map(|reader| reader.l0_intervals())),
        }
    }

    planned
        .write_compacted_to(&plan, &InMemoryIo::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn continuation_frames_compact_to_single_frames() {
    let archive = hand_built();
    let reader = Reader::open(archive.io()).await.unwrap();
    let out = compact(&reader).await;

    let (index, data) = reference_split_day(&archive.day, DEFAULT_PAGES_PER_FILE);
    assert!(file_bytes(&out, FileRole::Index).await == index);
    assert!(data_files(&out).await == data);

    let parsed = parse_index(&index, 1024);
    assert_eq!(parsed.counts(), [4]);
    assert_eq!(
        parsed.intervals,
        [(1, 10, l0_frame_len(1) as u32)],
        "two source intervals are one compact interval"
    );
    assert_eq!(le_u32_at(&index, 4), 0, "one L0 frame");

    for page in data[0].chunks(DATA_PAGE) {
        assert_eq!(
            (le_u32_at(page, 0) as usize, le_u32_at(page, 4)),
            (DATA_PAGE, 0),
            "every data page is one frame"
        );
    }
}

/// A source of `keys` logins, ten data pages per file, and its plan.
async fn planned_source(keys: u64) -> (InMemoryIo, Reader, CompactionPlan) {
    let rows: Vec<Row> = (1..=keys)
        .map(|login| (login, 2, Record::with(login)))
        .collect();
    let io = build(&rows, 1024, 16, 8, Some(10)).await;

    let reader = Reader::open(io.clone()).await.unwrap();
    reader.set_max_data_file_bytes(10 * DATA_PAGE as u64);
    let plan = reader.compaction_plan().await.unwrap();

    (io, reader, plan)
}

async fn assert_no_index(out: InMemoryIo, case: &str) {
    assert!(out.file(FileRole::Index).is_none(), "{case}");
    assert!(!out.created_roles().contains(&FileRole::Index), "{case}");

    match Reader::open(out).await {
        Err(ArchiveError::Io(err)) => {
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{case}")
        }
        other => panic!("{case}: {:?}", other.map(|reader| reader.l0_intervals())),
    }
}

#[tokio::test]
async fn a_compaction_that_fails_in_its_data_pass_leaves_no_index() {
    // A source data page that cannot be read, halfway through the source.
    let (source, reader, plan) = planned_source(40).await;
    source.file(FileRole::Data(2)).unwrap().fail_reads();

    let out = InMemoryIo::default();
    assert!(matches!(
        reader.write_compacted_to(&plan, &out).await,
        Err(ArchiveError::Io(_))
    ));
    assert_eq!(out.created_roles(), [FileRole::Data(0), FileRole::Data(1)]);
    assert_no_index(out, "a failed source read").await;

    // A destination data file that cannot be appended to after its first pages.
    let (_, reader, plan) = planned_source(40).await;
    let out = InMemoryIo::default();
    out.prepare(FileRole::Data(3)).fail_appends_after(4);

    assert!(matches!(
        reader.write_compacted_to(&plan, &out).await,
        Err(ArchiveError::Io(_))
    ));
    assert_no_index(out, "a failed destination append").await;

    // A destination data file whose finish fails: nothing past it is written.
    let (_, reader, plan) = planned_source(40).await;
    let out = InMemoryIo::default();
    out.prepare(FileRole::Data(3)).fail_finish();

    assert!(matches!(
        reader.write_compacted_to(&plan, &out).await,
        Err(ArchiveError::Io(_))
    ));
    assert_no_index(out, "a failed data file finish").await;
}

#[tokio::test]
async fn an_index_pass_that_fails_after_the_l0_frame_leaves_an_index_open_refuses() {
    // (keys, appends that go through: the L0 frame, then that many L1 pages)
    for (keys, succeeding) in [(40, 1), (2_500, 1), (2_500, 2), (2_500, 3)] {
        let (_, reader, plan) = planned_source(keys).await;
        let out = InMemoryIo::default();
        let index = out.prepare(FileRole::Index);
        index.fail_appends_after(succeeding);

        let result = reader.write_compacted_to(&plan, &out).await;
        let case = format!("{keys} keys, {succeeding} appends");
        assert!(matches!(result, Err(ArchiveError::Io(_))), "{case}");

        for no in 0..plan.data_file_sizes().len() {
            let file = out.file(FileRole::Data(no as u8)).unwrap();
            assert_eq!(file.finishes(), 1, "{case}: data file {no}");
        }

        wait_until("the finish spawned by drop", || index.finishes() == 1).await;
        assert_eq!(index.finishes_spawned_by_drop(), 1, "{case}");
        assert!(index.bytes().await.len() < plan.index_size() as usize);

        match Reader::open(out).await {
            Err(ArchiveError::Corrupted(message)) => {
                assert!(message.contains("past the end"), "{case}: {message}")
            }
            other => panic!("{case}: {:?}", other.map(|reader| reader.l0_intervals())),
        }
    }
}
