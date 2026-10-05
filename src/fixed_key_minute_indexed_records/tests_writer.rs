use super::test_support::{
    DAY, FRAME, L2, MINUTE, Model, NAME, Writer, build, day, ids, open, parse_archive, record,
    refused, time,
};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::test_items::Login;
use crate::{MAX_RECORD_LEN, RECORD_TIME_LEN};

#[tokio::test]
async fn records_written_out_of_order_come_back_in_time_order() {
    // (id, minute, microseconds) in the order written.
    let written = [
        (1, 30, 0),
        (2, 10, 5),
        (3, 1439, 59_999_999),
        (4, 0, 0),
        (5, 10, 1),
        (6, 30, 0),
        (7, 29, 59_999_999),
        (8, 10, 5),
    ];

    let rows: Vec<(u64, Vec<u8>)> = written
        .iter()
        .map(|(id, minute, micros)| (77, record(time(*minute, *micros), *id, 40)))
        .collect();

    let io = build(&rows, 1024, 64).await;
    let reader = open(&io).await;

    // Equal times keep the order they were written in: 2 before 8 at 10:00.000005, 1 before 6 at 30:00.
    let expected = vec![4, 5, 2, 8, 7, 1, 6, 3];
    assert_eq!(ids(&reader.get_day(&Login(77)).await.unwrap()), expected);
    assert_eq!(
        Model::of(&rows).day(77),
        reader.get_day(&Login(77)).await.unwrap()
    );

    let parsed = parse_archive(&io).await;
    let key = &parsed[&77];
    assert_eq!(key.frames.len(), 1);

    let at = |index: usize| 10 + index as u32 * 42;
    assert_eq!(key.cells[0], Some((0, 0, at(0))));
    assert_eq!(key.cells[10], Some((0, 0, at(1))));
    assert_eq!(key.cells[29], Some((0, 0, at(4))));
    assert_eq!(key.cells[30], Some((0, 0, at(5))));
    assert_eq!(key.cells[1439], Some((0, 0, at(7))));
    assert_eq!(key.cells.iter().flatten().count(), 5);
}

#[tokio::test]
async fn equal_times_keep_arrival_order_across_frames_and_keys() {
    let t = time(600, 123);
    let mut rows = Vec::new();

    // 20 records of one time, 3 KiB each, so they span frames; an earlier and a later one written between.
    for id in 0..20 {
        rows.push((5, record(t, id, 3_000)));

        if id == 7 {
            rows.push((5, record(t - 1, 100, 3_000)));
            rows.push((5, record(t + 1, 101, 3_000)));
        }

        rows.push((6, record(t, 200 + id, 16)));
    }

    let io = build(&rows, 1024, 2).await;
    let reader = open(&io).await;

    let mut expected = vec![100];
    expected.extend(0..20);
    expected.push(101);
    assert_eq!(ids(&reader.get_day(&Login(5)).await.unwrap()), expected);
    assert_eq!(
        ids(&reader.get_day(&Login(6)).await.unwrap()),
        (200..220).collect::<Vec<u64>>()
    );

    assert!(parse_archive(&io).await[&5].frames.len() > 2);
}

#[tokio::test]
async fn an_empty_minute_is_all_ff_and_only_minutes_with_records_have_cells() {
    let rows = vec![
        (1, record(time(0, 0), 1, 20)),
        (1, record(time(719, 30), 2, 20)),
        (1, record(time(1439, 0), 3, 20)),
    ];

    let io = build(&rows, 1024, 64).await;
    let index = io.file(FileRole::Index).unwrap().bytes().await;
    let key = &parse_archive(&io).await[&1];

    let l2 = key.l2_offset as usize;
    assert_eq!(&index[l2..l2 + 8], &[0xA8, 0x32, 0, 0, 0, 0, 0, 0]);

    for minute in 0..1440 {
        let cell = &index[l2 + 8 + minute * 9..l2 + 17 + minute * 9];

        if [0, 719, 1439].contains(&minute) {
            assert_ne!(cell, [0xFF; 9], "minute {minute}");
        } else {
            assert_eq!(cell, [0xFF; 9], "minute {minute}");
        }
    }
}

#[tokio::test]
async fn a_working_archive_is_one_data_file_of_whole_frames_and_its_index_grows_by_key() {
    let rows: Vec<(u64, Vec<u8>)> = (0..50)
        .map(|n| (n % 5, record(time(n as i64, 0), n, 4_000)))
        .collect();

    let io = build(&rows, 2, 1).await;

    assert_eq!(io.roles(), [FileRole::Index, FileRole::Data(0)]);

    let data = io.file(FileRole::Data(0)).unwrap().bytes().await;
    assert_eq!(data.len() % FRAME, 0);

    let reader = open(&io).await;
    assert_eq!(reader.keys_count().await.unwrap(), 5);
    assert_eq!(reader.l0_intervals(), 3);

    let parsed = parse_archive(&io).await;
    for (login, key) in &parsed {
        assert_eq!(key.records.len(), 10, "login {login}");
        assert!(
            key.frames
                .iter()
                .all(|frame| frame.size as usize == FRAME && frame.file_no == 0)
        );
    }

    let index = io.file(FileRole::Index).unwrap().bytes().await;
    assert!(index.len() > 5 * L2);
}

#[tokio::test]
async fn a_record_shorter_than_its_time_or_longer_than_the_limit_is_refused_and_changes_nothing() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone(), NAME, day()).await.unwrap();

    writer
        .write(Login(1), &record(time(1, 0), 1, 100))
        .await
        .unwrap();

    for len in [0, RECORD_TIME_LEN - 1, MAX_RECORD_LEN + 1] {
        let message = refused(writer.write(Login(1), &vec![0; len]).await);
        assert!(
            message.contains(&format!("a record of {len} bytes")),
            "{message}"
        );
    }

    writer
        .write(Login(1), &record(time(2, 0), 2, MAX_RECORD_LEN))
        .await
        .unwrap();
    writer
        .write(Login(2), &record(time(3, 0), 3, RECORD_TIME_LEN))
        .await
        .unwrap();
    assert_eq!((writer.keys_count(), writer.records_count()), (2, 3));

    writer.finish().await.unwrap();

    let reader = open(&io).await;
    assert_eq!(reader.get_day(&Login(1)).await.unwrap().len(), 2);
    assert_eq!(reader.get_day(&Login(2)).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_key_needing_index_room_past_the_limit_is_refused_and_the_writer_stays_usable() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), NAME, day(), 4, 4)
        .await
        .unwrap();

    // Frame 0 (20 + 4 × 20), one L1 page (12 + 4 × 12) and two L2 frames.
    let limit = (100 + 60 + 2 * L2) as u64;
    writer.set_max_index_bytes(limit);

    writer.write(Login(1), &record(DAY, 1, 50)).await.unwrap();
    writer.write(Login(2), &record(DAY, 2, 50)).await.unwrap();

    let message = refused(writer.write(Login(3), &record(DAY, 3, 50)).await);
    assert!(message.contains("would grow the index past"), "{message}");

    // A known key needs no index room.
    writer
        .write(Login(1), &record(DAY + 5, 4, 50))
        .await
        .unwrap();
    writer.finish().await.unwrap();

    let reader = open(&io).await;
    assert_eq!(reader.keys_count().await.unwrap(), 2);
    assert_eq!(ids(&reader.get_day(&Login(1)).await.unwrap()), [1, 4]);
    assert_eq!(
        reader.get_day(&Login(3)).await.unwrap(),
        Vec::<Vec<u8>>::new()
    );
}

/// Every new key reserves its L2 frame, so a key that fits a page already there is counted with the L0
/// continuation a flush will need too — or it takes that room, and the flush after it runs into the limit.
/// Found in review, as a flush refused after two keys were accepted.
#[tokio::test]
async fn a_key_that_fits_an_existing_page_is_refused_when_it_would_take_the_room_of_a_pending_continuation()
 {
    const CAPACITY_L0: usize = 700;
    const L0_FRAME: u64 = 20 + CAPACITY_L0 as u64 * 20;
    const L1_PAGE: u64 = 12 + 2 * 12;

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), NAME, day(), CAPACITY_L0, 2)
        .await
        .unwrap();

    // 1 400 keys in ascending order, two per page: 700 pages fill frame 0 exactly.
    for key in (0..2_800u64).step_by(2) {
        writer
            .write(Login(key), &record(time(1, 0), key, 40))
            .await
            .unwrap();
    }
    assert_eq!((writer.l0_intervals(), writer.l0_frames()), (700, 1));

    let reserved = writer.index.reserved();
    assert_eq!(reserved, L0_FRAME + 1_400 * L2 as u64 + 700 * L1_PAGE);

    // Room for key 2800 — its L2 frame, its new page, the continuation interval 701 needs — and one byte short
    // of one more L2 frame beside that continuation.
    writer.set_max_index_bytes(reserved + L2 as u64 + L1_PAGE + L0_FRAME + L2 as u64 - 1);

    writer
        .write(Login(2_800), &record(time(1, 0), 2_800, 40))
        .await
        .unwrap();
    assert_eq!(writer.l0_intervals(), 701);

    // Key 2801 fits page 701, and its L2 frame would leave no room for the continuation.
    let message = refused(
        writer
            .write(Login(2_801), &record(time(1, 0), 2_801, 40))
            .await,
    );
    assert!(message.contains("would grow the index past"), "{message}");

    // The writer is usable, and the flush that reserves the continuation fits.
    writer
        .write(Login(0), &record(time(2, 0), 9_999, 40))
        .await
        .unwrap();
    writer.flush().await.unwrap();
    assert_eq!(writer.l0_frames(), 2);
    writer.finish().await.unwrap();

    let reader = open(&io).await;
    assert_eq!(reader.keys_count().await.unwrap(), 1_401);
    assert_eq!(reader.l0_frames(), 2);
    assert_eq!(ids(&reader.get_day(&Login(0)).await.unwrap()), [0, 9_999]);
    assert_eq!(ids(&reader.get_day(&Login(2_800)).await.unwrap()), [2_800]);
    assert!(reader.get_day(&Login(2_801)).await.unwrap().is_empty());
}

#[tokio::test]
#[should_panic(expected = "archive mt5-test 2026-09-15: a record at 2026-09-14T23:59:59.999999")]
async fn a_record_before_the_day_panics_naming_the_archive_and_the_day() {
    let mut writer = Writer::new(InMemoryIo::default(), NAME, day())
        .await
        .unwrap();
    let _ = writer.write(Login(1), &record(DAY - 1, 1, 20)).await;
}

#[tokio::test]
#[should_panic(expected = "is outside its day, which starts at 2026-09-15T00:00:00.000000")]
async fn a_record_after_the_day_panics() {
    let mut writer = Writer::new(InMemoryIo::default(), NAME, day())
        .await
        .unwrap();
    writer.write(Login(1), &record(DAY, 1, 20)).await.unwrap();
    let _ = writer
        .write(Login(1), &record(DAY + 1440 * MINUTE, 2, 20))
        .await;
}

#[tokio::test]
#[should_panic(expected = "archive mt5-test 2026-09-15: its data file is 98304 bytes")]
async fn a_data_frame_past_the_data_file_limit_panics_naming_the_archive_and_the_file_size() {
    let mut writer = Writer::new(InMemoryIo::default(), NAME, day())
        .await
        .unwrap();
    writer.set_max_data_file_bytes(3 * FRAME as u64);

    for login in 0..3 {
        writer
            .write(Login(login), &record(DAY, login, 100))
            .await
            .unwrap();
    }

    let _ = writer.write(Login(9), &record(DAY, 9, 100)).await;
}

#[tokio::test]
#[should_panic(
    expected = "its data file is 32768 bytes, and 32768 more for a data frame would take it past 32768 bytes"
)]
async fn a_frame_a_known_key_needs_past_the_limit_panics_too() {
    let mut writer = Writer::new(InMemoryIo::default(), NAME, day())
        .await
        .unwrap();
    writer.set_max_data_file_bytes(FRAME as u64);

    for n in 0..9 {
        let _ = writer
            .write(Login(1), &record(time(n, 0), n as u64, 4_000))
            .await;
    }
}

#[tokio::test]
async fn nothing_is_written_to_a_data_file_before_the_first_record() {
    let io = InMemoryIo::default();
    let writer = Writer::new(io.clone(), NAME, day()).await.unwrap();

    assert_eq!(io.created_roles(), [FileRole::Index]);
    writer.finish().await.unwrap();

    let reader = open(&io).await;
    assert_eq!(reader.keys_count().await.unwrap(), 0);
    assert!(matches!(
        reader.get_day(&Login(1)).await,
        Ok(records) if records.is_empty()
    ));
}
