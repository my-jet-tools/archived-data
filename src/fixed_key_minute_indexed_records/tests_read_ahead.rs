//! A walk reads its frames through a window that grows while the chain runs back to back through its file —
//! a compact key's run — and falls back to one frame when the chain jumps, as a working chain does. Found in
//! review: a day of 17 000 records was 173 reads, one a frame.

use super::test_support::{
    FRAME, L2, MINUTE, Model, Reader, build, compact, day, ids, moment, open, parse_archive,
    record, seeded, time,
};
use crate::MAX_RECORDS_READ_AHEAD;
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::test_items::Login;

/// Key 1: `records` records of 300 to 359 bytes spread over the day, in random order; keys 2 to 5: 20 each.
fn day_of_one_busy_key(records: u64) -> Vec<(u64, Vec<u8>)> {
    let mut rng = seeded(records);
    let mut rows = Vec::new();

    for id in 0..records {
        let at = time(
            (rng.next() % 1440) as i64,
            (rng.next() % MINUTE as u64) as i64,
        );
        rows.push((1, record(at, id, 300 + (rng.next() % 60) as usize)));
    }

    for id in 0..80 {
        let at = time(
            (rng.next() % 1440) as i64,
            (rng.next() % MINUTE as u64) as i64,
        );
        rows.push((2 + id % 4, record(at, 100_000 + id, 330)));
    }

    rows
}

/// `(read_at`s, bytes read, the largest read)` of data file 0 while `read` runs.
async fn measured<T>(io: &InMemoryIo, read: impl Future<Output = T>) -> (T, (usize, usize, usize)) {
    let file = io.file(FileRole::Data(0)).unwrap();
    let before = (file.read_ats(), file.bytes_read());
    let answer = read.await;

    let reads = file.read_ats() - before.0;
    let bytes = file.bytes_read() - before.1;
    (answer, (reads, bytes, file.largest_read()))
}

#[tokio::test]
async fn a_compact_run_of_many_frames_is_a_few_reads_none_longer_than_the_window() {
    let rows = day_of_one_busy_key(6_000);
    let model = Model::of(&rows);

    let compacted = compact(&open(&build(&rows, 1024, 64).await).await).await;
    let parsed = parse_archive(&compacted).await;
    let run = &parsed[&1].frames;
    let run_len = (run.last().unwrap().offset + run.last().unwrap().size - run[0].offset) as usize;
    assert!(run.len() > 50, "{} frames", run.len());

    let reader = open(&compacted).await;
    reader.set_data_cache_size(0).await;

    let (read, (reads, bytes, largest)) = measured(&compacted, reader.get_day(&Login(1))).await;
    assert!(read.unwrap() == model.day(1));

    // 32, 64, 128, 256 KiB, then 512 KiB reads: under 10 for about 2 MiB of frames.
    assert!(reads <= 10, "{reads} reads for {} frames", run.len());
    assert!(
        largest <= MAX_RECORDS_READ_AHEAD,
        "a read of {largest} bytes"
    );
    assert!(
        bytes <= 2 * run_len + MAX_RECORDS_READ_AHEAD,
        "{bytes} bytes for {run_len}"
    );

    // A key of one frame is one read of at most a frame.
    for login in 2..=5 {
        let (read, (reads, bytes, _)) = measured(&compacted, reader.get_day(&Login(login))).await;
        assert!(read.unwrap() == model.day(login));
        assert_eq!(reads, 1, "login {login}");
        assert!(bytes <= FRAME, "login {login}: {bytes} bytes");
    }

    // A range starts at its first minute's frame, not at the start of the run.
    let from = time(720, 0);
    let to = time(780, 0);
    let (read, (reads, _, _)) = measured(
        &compacted,
        reader.get_range(&Login(1), moment(720, 0), moment(780, 0)),
    )
    .await;
    assert!(read.unwrap() == model.range(1, from, to));
    assert!(reads <= 3, "{reads} reads for one hour");
}

#[tokio::test]
async fn every_read_through_the_window_is_what_the_model_holds_with_and_without_a_cache() {
    let rows = day_of_one_busy_key(2_500);
    let model = Model::of(&rows);

    let working = build(&rows, 2, 3).await;
    let compacted = compact(&open(&working).await).await;

    for (case, io) in [("working", &working), ("compact", &compacted)] {
        for cache in [0, 1, 10] {
            let reader = open(io).await;
            reader.set_data_cache_size(cache).await;

            let mut rng = seeded(cache as u64 + 77);

            for _ in 0..60 {
                let login = 1 + rng.next() % 5;
                let from = (rng.next() % 1440) as i64;
                let to = from + (rng.next() % 400) as i64;

                let read = reader
                    .get_range(&Login(login), moment(from, 0), moment(to, 0))
                    .await
                    .unwrap();
                assert!(
                    read == model.range(login, time(from, 0), time(to, 0)),
                    "{case}, cache {cache}: login {login}, minutes {from}..={to}"
                );
            }

            for login in 1..=5 {
                assert!(reader.get_day(&Login(login)).await.unwrap() == model.day(login));
            }
        }
    }
}

#[tokio::test]
async fn a_working_chain_that_jumps_back_through_its_file_reads_a_frame_at_a_time() {
    // Frames 0 and 1 hold records 1 ..= 13; record 20 halves frame 0 into a new frame 2 chained between them:
    // the chain is 0 → 2 → 1.
    let mut rows: Vec<(u64, Vec<u8>)> = (1..=13)
        .map(|n| (1, record(time(n, 10_000_000), n as u64, 4_000)))
        .collect();
    rows.push((1, record(time(2, 30_000_000), 20, 4_000)));

    let io = build(&rows, 1024, 64).await;
    let frames: Vec<u32> = parse_archive(&io).await[&1]
        .frames
        .iter()
        .map(|frame| frame.offset)
        .collect();
    assert_eq!(frames, [0, 2 * FRAME as u32, FRAME as u32]);

    let reader = open(&io).await;
    reader.set_data_cache_size(0).await;

    let (read, (reads, bytes, _)) = measured(&io, reader.get_day(&Login(1))).await;
    assert_eq!(
        ids(&read.unwrap()),
        [1, 2, 20, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]
    );
    assert_eq!((reads, bytes), (3, 3 * FRAME));

    // Frames appended one after another in the order the chain walks them: the window grows.
    let rows: Vec<(u64, Vec<u8>)> = (0..40)
        .map(|n| (1, record(time(n, 0), n as u64, 4_000)))
        .collect();
    let io = build(&rows, 1024, 64).await;
    assert_eq!(parse_archive(&io).await[&1].frames.len(), 5);

    let reader = open(&io).await;
    reader.set_data_cache_size(0).await;

    let (read, (reads, _, _)) = measured(&io, reader.get_day(&Login(1))).await;
    assert_eq!(read.unwrap().len(), 40);
    assert_eq!(
        reads, 3,
        "32 KiB, then 64 KiB, then 128 KiB clamped to the file"
    );
}

#[tokio::test]
async fn a_frame_longer_than_a_read_is_read_whole_with_one_more_read() {
    // One key, five records of 8 000 bytes at minutes 0 to 4, compacted into frames of 4 and 1; then rewritten
    // by hand as ONE frame of 40 020 bytes, which no writer of this crate makes, and its cells patched to it.
    let rows: Vec<(u64, Vec<u8>)> = (0..5)
        .map(|n| (1, record(time(n, 0), n as u64, 8_000)))
        .collect();
    let model = Model::of(&rows);

    let compacted = compact(&open(&build(&rows, 1024, 64).await).await).await;
    let mut index = compacted.file(FileRole::Index).unwrap().bytes().await;
    let data = compacted.file(FileRole::Data(0)).unwrap().bytes().await;

    let records: Vec<u8> = [&data[10..32_018], &data[32_018 + 10..]].concat();
    assert_eq!(records.len(), 5 * 8_002);

    let mut one_frame = (10 + records.len() as u32).to_le_bytes().to_vec();
    one_frame.extend_from_slice(&0u32.to_le_bytes());
    one_frame.extend_from_slice(&(records.len() as u16).to_le_bytes());
    one_frame.extend_from_slice(&records);

    let l2 = 20 + 20;
    assert_eq!(index[l2..l2 + 4], (L2 as u32).to_le_bytes());
    let cell = l2 + 8 + 4 * 9;
    index[cell + 1..cell + 5].copy_from_slice(&0u32.to_le_bytes());
    index[cell + 5..cell + 9].copy_from_slice(&(10u32 + 4 * 8_002).to_le_bytes());

    let io = InMemoryIo::default();
    io.put(FileRole::Index, index);
    io.put(FileRole::Data(0), one_frame);

    let reader = Reader::open(io.clone(), day()).await.unwrap();
    reader.set_data_cache_size(0).await;

    let (read, (reads, bytes, _)) = measured(&io, reader.get_day(&Login(1))).await;
    assert!(read.unwrap() == model.day(1));
    assert_eq!(
        (reads, bytes),
        (2, 40_020),
        "a read of 32 KiB, then the rest"
    );
}

#[tokio::test]
async fn a_data_file_cut_inside_a_run_is_corrupted_for_that_key_and_not_for_one_before_it() {
    let rows = day_of_one_busy_key(1_000);
    let model = Model::of(&rows);

    let compacted = compact(&open(&build(&rows, 1024, 64).await).await).await;
    let index = compacted.file(FileRole::Index).unwrap().bytes().await;
    let data = compacted.file(FileRole::Data(0)).unwrap().bytes().await;

    // Key 1's run lies first; key 5's last.
    let io = InMemoryIo::default();
    io.put(FileRole::Index, index);
    io.put(FileRole::Data(0), data[..data.len() - 100].to_vec());

    let reader = open(&io).await;
    assert!(reader.get_day(&Login(1)).await.unwrap() == model.day(1));
    assert!(matches!(
        reader.get_day(&Login(5)).await,
        Err(crate::ArchiveError::Corrupted(_))
    ));
}
