use std::collections::BTreeMap;

use super::test_support::{
    DAY, FRAME, L2, MINUTE, Model, Reader, assert_reads_back, build, compact, data_files, day,
    file_bytes, le_u32_at, le_u64_at, moment, open, parse_archive, random_rows, record, seeded,
    time,
};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::test_items::Login;
use crate::{ArchiveError, DEFAULT_MAX_DATA_FILE_BYTES};

/// Every read a caller can make of `logins`, answered by `reader`: each day, and a spread of ranges.
async fn reads(reader: &Reader, logins: &[u64]) -> Vec<Vec<Vec<u8>>> {
    let mut answers = Vec::new();

    for login in logins {
        answers.push(reader.get_day(&Login(*login)).await.unwrap());

        for (from, to) in [(0, 30), (29, 31), (100, 700), (700, 1439), (-5, 2000)] {
            answers.push(
                reader
                    .get_range(&Login(*login), moment(from, 0), moment(to, 0))
                    .await
                    .unwrap(),
            );
        }
    }

    answers
}

#[tokio::test]
async fn a_compact_archive_answers_every_read_exactly_as_the_working_one_did() {
    let mut rng = seeded(0xC0FFEE);
    let rows = random_rows(40, 4_000, &mut rng);
    let model = Model::of(&rows);
    let logins: Vec<u64> = model.logins.keys().copied().chain([1, 5_000]).collect();

    let working = open(&build(&rows, 4, 5).await).await;
    let compacted = compact(&working).await;
    let reader = open(&compacted).await;

    assert!(reads(&reader, &logins).await == reads(&working, &logins).await);
    assert_reads_back(&reader, &model, "compact").await;

    assert_eq!(reader.keys_count().await.unwrap(), model.logins.len());
    assert_eq!(reader.l0_frames(), 1);
    assert_eq!(compacted.roles(), [FileRole::Index, FileRole::Data(0)]);

    // Every key one run of frames back to back, cut to their records, chained forward.
    let parsed = parse_archive(&compacted).await;
    let data = file_bytes(&compacted, FileRole::Data(0)).await;
    let mut end = 0;

    for key in parsed.values() {
        assert!(
            key.frames[0].offset == end,
            "runs follow one another in key order"
        );

        for pair in key.frames.windows(2) {
            assert_eq!(pair[0].next, pair[0].offset + pair[0].size);
        }

        for frame in &key.frames {
            let used = u16::from_le_bytes(
                data[frame.offset as usize + 8..frame.offset as usize + 10]
                    .try_into()
                    .unwrap(),
            );
            assert_eq!(frame.size, 10 + used as u32, "no free space");
            assert!(frame.size as usize <= FRAME);
        }

        let last = key.frames.last().unwrap();
        assert_eq!(last.next, 0);
        end = last.offset + last.size;
    }
}

#[tokio::test]
async fn compaction_depends_only_on_the_records_whatever_order_and_frames_they_came_in() {
    let mut rng = seeded(99);

    // Every time distinct, so every order of the rows describes the same day.
    let mut rows: Vec<(u64, Vec<u8>)> = random_rows(10, 1_500, &mut rng)
        .into_iter()
        .enumerate()
        .map(|(id, (login, bytes))| {
            let minute = (le_u64_at(&bytes, 0) as i64 - DAY) / MINUTE;
            (
                login,
                record(time(minute, id as i64), id as u64, bytes.len()),
            )
        })
        .collect();

    let first = compact(&open(&build(&rows, 1024, 64).await).await).await;

    rng.shuffle(&mut rows);
    let second = compact(&open(&build(&rows, 2, 1).await).await).await;

    rows.reverse();
    let third = compact(&open(&build(&rows, 5, 0).await).await).await;

    for other in [&second, &third] {
        assert!(
            file_bytes(other, FileRole::Index).await == file_bytes(&first, FileRole::Index).await
        );
        assert!(data_files(other).await == data_files(&first).await);
    }

    // A compact archive compacts to itself.
    let again = compact(&open(&first).await).await;
    assert!(file_bytes(&again, FileRole::Index).await == file_bytes(&first, FileRole::Index).await);
    assert!(data_files(&again).await == data_files(&first).await);
}

#[tokio::test]
async fn the_plan_sizes_are_exactly_what_is_written_data_files_first_and_the_index_last() {
    let mut rng = seeded(4);
    let rows = random_rows(30, 2_000, &mut rng);

    let reader = open(&build(&rows, 1024, 64).await).await;
    reader.set_max_data_file_bytes(200_000);
    let plan = reader.compaction_plan().await.unwrap();

    assert_eq!(plan.keys_count(), reader.keys_count().await.unwrap());
    assert_eq!(plan.records_count(), 2_000);

    let pages = plan.keys_count().div_ceil(1024);
    assert_eq!(
        plan.index_size(),
        (20 + pages * 20 + plan.keys_count() * L2 + pages * (12 + 1024 * 12)) as u64
    );

    let out = InMemoryIo::default();
    reader.write_compacted_to(&plan, &out).await.unwrap();

    let files = plan.data_file_sizes().len();
    assert!(files > 1);

    let mut roles: Vec<FileRole> = (0..files as u8).map(FileRole::Data).collect();
    roles.push(FileRole::Index);
    assert_eq!(
        out.created_roles(),
        roles,
        "each data file in order, then the index"
    );

    for (no, size) in plan.data_file_sizes().iter().enumerate() {
        assert_eq!(
            file_bytes(&out, FileRole::Data(no as u8)).await.len() as u64,
            *size
        );
    }
    assert_eq!(
        file_bytes(&out, FileRole::Index).await.len() as u64,
        plan.index_size()
    );

    for role in out.roles() {
        let file = out.file(role).unwrap();
        assert_eq!(
            (file.write_ats(), file.finishes()),
            (0, 1),
            "{role:?} is appended only and finished once"
        );
    }
}

#[tokio::test]
async fn a_compact_data_file_ends_only_between_keys() {
    let mut rng = seeded(11);
    let rows = random_rows(50, 3_000, &mut rng);
    let model = Model::of(&rows);

    let working = open(&build(&rows, 16, 8).await).await;
    let max = 150_000u64;
    working.set_max_data_file_bytes(max);

    let compacted = compact(&working).await;
    let reader = open(&compacted).await;
    assert_reads_back(&reader, &model, "rolled over").await;

    let parsed = parse_archive(&compacted).await;
    let sizes: Vec<u64> = data_files(&compacted)
        .await
        .iter()
        .map(|file| file.len() as u64)
        .collect();
    assert!(sizes.len() > 3, "{} data files", sizes.len());

    // (file, run start, run length) of every key, in key order.
    let runs: Vec<(u8, u32, u32)> = parsed
        .values()
        .map(|key| {
            let file = key.frames[0].file_no;
            assert!(
                key.frames.iter().all(|frame| frame.file_no == file),
                "one key, one file"
            );

            let last = key.frames.last().unwrap();
            (
                file,
                key.frames[0].offset,
                last.offset + last.size - key.frames[0].offset,
            )
        })
        .collect();

    for pair in runs.windows(2) {
        let ((file, start, len), (next_file, next_start, next_len)) = (pair[0], pair[1]);

        if next_file == file {
            assert_eq!(next_start, start + len, "back to back in one file");
        } else {
            assert_eq!(next_file, file + 1);
            assert_eq!(next_start, 0);
            assert_eq!(
                sizes[file as usize],
                (start + len) as u64,
                "the file ends with the key"
            );
            assert!(
                (start + len + next_len) as u64 > max,
                "the next key would not have fit"
            );
        }
    }

    for size in sizes {
        assert!(size <= max);
    }

    // The cells of the compact archive point into the data file each key lies in.
    for key in parsed.values() {
        let file = key.frames[0].file_no;
        assert!(key.cells.iter().flatten().all(|(no, _, _)| *no == file));
    }
}

#[tokio::test]
async fn a_key_longer_than_a_data_file_is_a_contract_violation_of_the_plan() {
    let rows: Vec<(u64, Vec<u8>)> = (0..20)
        .map(|n| (3, record(time(n, 0), n as u64, 8_000)))
        .collect();

    let reader = open(&build(&rows, 1024, 64).await).await;
    reader.set_max_data_file_bytes(FRAME as u64 * 4);

    assert!(matches!(
        reader.compaction_plan().await,
        Err(ArchiveError::ContractViolation(message)) if message.contains("more than a data file")
    ));

    reader.set_max_data_file_bytes(DEFAULT_MAX_DATA_FILE_BYTES);
    assert!(reader.compaction_plan().await.is_ok());
}

#[tokio::test]
async fn a_source_that_changed_since_its_plan_is_corrupted() {
    let rows = vec![
        (1, record(time(1, 0), 1, 100)),
        (2, record(time(2, 0), 2, 100)),
    ];
    let reader = open(&build(&rows, 1024, 64).await).await;
    let plan = reader.compaction_plan().await.unwrap();

    let mut more = rows.clone();
    more.push((2, record(time(3, 0), 3, 100)));
    let changed = open(&build(&more, 1024, 64).await).await;

    let out = InMemoryIo::default();
    assert!(matches!(
        changed.write_compacted_to(&plan, &out).await,
        Err(ArchiveError::Corrupted(message)) if message.contains("changed while it was being compacted")
    ));
    assert!(
        out.file(FileRole::Index).is_none(),
        "no index is left behind"
    );
}

#[tokio::test]
async fn an_index_cut_by_a_failed_compaction_is_refused_at_open() {
    let mut rng = seeded(21);
    let rows = random_rows(5, 300, &mut rng);
    let compacted = compact(&open(&build(&rows, 1024, 64).await).await).await;

    let index = file_bytes(&compacted, FileRole::Index).await;
    let data = data_files(&compacted).await;

    let l2_end = 20 + 20 + 5 * L2;
    for cut in [l2_end - 1, l2_end, l2_end + 12, index.len() - 1] {
        let io = InMemoryIo::default();
        io.put(FileRole::Index, index[..cut].to_vec());
        io.put(FileRole::Data(0), data[0].clone());

        assert!(
            matches!(
                Reader::open(io, day()).await,
                Err(ArchiveError::Corrupted(_))
            ),
            "an index cut at {cut} of {}",
            index.len()
        );
    }

    let io = InMemoryIo::default();
    io.put(FileRole::Index, index.clone());
    io.put(FileRole::Data(0), data[0].clone());
    let reader = Reader::open(io, day()).await.unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), 5);
    assert_eq!(
        le_u32_at(&index, 40) as usize,
        L2,
        "the L2 frames follow L0"
    );

    let by_key: BTreeMap<u64, usize> = parse_archive(&compacted)
        .await
        .into_iter()
        .map(|(login, key)| (login, key.records.len()))
        .collect();
    assert_eq!(by_key.values().sum::<usize>(), 300);
}
