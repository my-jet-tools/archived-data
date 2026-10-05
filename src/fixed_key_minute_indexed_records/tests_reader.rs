use std::sync::Arc;

use rust_extensions::date_time::DateTimeAsMicroseconds;

use super::test_support::{
    DAY, MINUTE, Model, NAME, Reader, Writer, assert_reads_back, build, data_read_ats, day, ids,
    moment, open, parse_archive, random_rows, record, seeded, time, write_all,
};
use crate::ArchiveError;
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::test_items::Login;

/// Key 5: records at 10:00, 10:59.999999, 11:00, 11:00 again, 20:30, and 1439:59.999999; minutes 12 to 19
/// hold none.
async fn edges() -> (InMemoryIo, Model) {
    let rows = vec![
        (5, record(time(11, 0), 3, 30)),
        (5, record(time(10, 0), 1, 30)),
        (5, record(time(20, 30_000_000), 5, 30)),
        (5, record(time(10, MINUTE - 1), 2, 30)),
        (5, record(time(11, 0), 4, 30)),
        (5, record(time(1439, MINUTE - 1), 6, 30)),
        (9, record(time(15, 0), 7, 30)),
    ];

    (build(&rows, 1024, 64).await, Model::of(&rows))
}

async fn range(reader: &Reader, from: i64, to: i64) -> Vec<u64> {
    let (from, to) = (
        DateTimeAsMicroseconds::new(from),
        DateTimeAsMicroseconds::new(to),
    );
    ids(&reader.get_range(&Login(5), from, to).await.unwrap())
}

#[tokio::test]
async fn a_range_is_inclusive_at_both_ends_to_the_microsecond() {
    let (io, model) = edges().await;
    let reader = open(&io).await;

    for (case, from, to, expected) in [
        ("one record exactly", time(10, 0), time(10, 0), vec![1]),
        (
            "past the first by one",
            time(10, 1),
            time(11, 0),
            vec![2, 3, 4],
        ),
        (
            "to the last microsecond of minute 10",
            time(10, 0),
            time(11, 0) - 1,
            vec![1, 2],
        ),
        ("equal times both in", time(11, 0), time(11, 0), vec![3, 4]),
        (
            "the whole minute 10",
            time(10, 0),
            time(10, MINUTE - 1),
            vec![1, 2],
        ),
        (
            "the last microsecond of the day",
            time(1439, MINUTE - 1),
            time(1439, MINUTE - 1),
            vec![6],
        ),
    ] {
        assert_eq!(range(&reader, from, to).await, expected, "{case}");
        assert_eq!(
            ids(&model.range(5, from, to)),
            expected,
            "{case}: the model agrees"
        );
    }
}

#[tokio::test]
async fn a_range_inside_an_empty_stretch_is_empty_and_one_reaching_out_of_it_finds_the_next_record()
{
    let (io, _) = edges().await;
    let reader = open(&io).await;

    assert_eq!(
        range(&reader, time(12, 0), time(19, 59)).await,
        Vec::<u64>::new()
    );
    assert_eq!(
        range(&reader, time(13, 7), time(20, 29_999_999)).await,
        Vec::<u64>::new()
    );
    assert_eq!(
        range(&reader, time(13, 7), time(20, 30_000_000)).await,
        vec![5]
    );
    assert_eq!(
        range(&reader, time(11, 1), time(20, 0)).await,
        Vec::<u64>::new()
    );
    assert_eq!(range(&reader, time(11, 1), time(1439, 0)).await, vec![5]);

    // Starting inside a minute that has records, after them.
    assert_eq!(
        range(&reader, time(10, 30), time(21, 0)).await,
        vec![2, 3, 4, 5]
    );
}

#[tokio::test]
async fn a_range_past_the_edges_of_the_day_is_clamped_to_it_and_one_outside_it_reads_nothing() {
    let (io, model) = edges().await;
    let reader = open(&io).await;

    assert_eq!(
        range(&reader, i64::MIN, i64::MAX).await,
        ids(&model.day(5)),
        "the whole of time"
    );
    assert_eq!(range(&reader, DAY - 1, time(10, 0)).await, vec![1]);

    let index = io.file(FileRole::Index).unwrap();
    let before = (index.read_ats(), data_read_ats(&io));

    for (from, to) in [
        (DAY - 10, DAY - 1),
        (time(1440, 0), time(2000, 0)),
        (time(30, 0), time(10, 0)),
        (i64::MIN, DAY - 1),
    ] {
        assert_eq!(
            range(&reader, from, to).await,
            Vec::<u64>::new(),
            "{from}..={to}"
        );
    }

    assert_eq!(
        (index.read_ats(), data_read_ats(&io)),
        before,
        "nothing was read"
    );
}

#[tokio::test]
async fn a_day_is_every_record_and_a_key_the_archive_does_not_hold_is_empty() {
    let (io, model) = edges().await;
    let reader = open(&io).await;

    assert_eq!(reader.get_day(&Login(5)).await.unwrap(), model.day(5));
    assert_eq!(ids(&reader.get_day(&Login(9)).await.unwrap()), [7]);

    for login in [0, 6, 10, u64::MAX] {
        assert!(reader.get_day(&Login(login)).await.unwrap().is_empty());
        assert!(
            reader
                .get_range(&Login(login), moment(0, 0), moment(1439, 0))
                .await
                .unwrap()
                .is_empty()
        );
    }

    let empty = build(&[], 1024, 64).await;
    let reader = open(&empty).await;
    assert!(reader.get_day(&Login(5)).await.unwrap().is_empty());
    assert_eq!(empty.roles(), [FileRole::Index]);
}

#[tokio::test]
async fn every_range_of_a_random_day_is_what_the_model_holds() {
    let mut rng = seeded(0x5EED);
    let rows = random_rows(12, 3_000, &mut rng);
    let model = Model::of(&rows);

    let io = build(&rows, 3, 7).await;
    let reader = Arc::new(open(&io).await);
    reader.set_data_cache_size(3).await;
    reader.set_l2_cache_size(2).await;

    parse_archive(&io).await;

    for login in model.logins.keys() {
        assert_eq!(
            reader.get_day(&Login(*login)).await.unwrap(),
            model.day(*login)
        );
    }

    for _ in 0..300 {
        let nth = (rng.next() % model.logins.len() as u64) as usize;
        let login = *model.logins.keys().nth(nth).unwrap();
        let a = DAY - MINUTE + (rng.next() % (1442 * MINUTE as u64)) as i64;
        let b = a + (rng.next() % (120 * MINUTE as u64)) as i64;

        assert!(
            reader
                .get_range(&Login(login), moment(0, a - DAY), moment(0, b - DAY))
                .await
                .unwrap()
                == model.range(login, a, b),
            "login {login}, {a}..={b}"
        );
    }
}

#[tokio::test]
async fn a_reader_told_another_day_refuses_what_it_reads() {
    let (io, _) = edges().await;

    let reader = Reader::open(io.clone(), moment(-1440, 0)).await.unwrap();
    assert!(matches!(
        reader.get_day(&Login(5)).await,
        Err(ArchiveError::Corrupted(message)) if message.contains("outside the archive's day")
    ));

    // A day starting one minute early: every record but the last is in it, under the wrong minute.
    let reader = Reader::open(io.clone(), moment(0, -MINUTE)).await.unwrap();
    assert!(matches!(
        reader.get_day(&Login(5)).await,
        Err(ArchiveError::Corrupted(message)) if message.contains("does not point at its first record")
    ));

    // Only a read that meets a record can tell: key 9 has one record, at minute 15, and a range whose minutes
    // start after it meets none — an empty answer, as it would be on the right day.
    let reader = Reader::open(io.clone(), moment(1440, 0)).await.unwrap();
    let none = reader
        .get_range(&Login(9), moment(1440 + 20, 0), moment(1440 + 30, 0))
        .await
        .unwrap();
    assert!(none.is_empty());
    assert!(matches!(
        reader.get_day(&Login(9)).await,
        Err(ArchiveError::Corrupted(_))
    ));
}

#[tokio::test]
async fn a_cell_pointing_elsewhere_or_a_chain_in_a_cycle_is_corrupted() {
    let rows: Vec<(u64, Vec<u8>)> = (0..12)
        .map(|n| (1, record(time(n, 0), n as u64, 4_000)))
        .collect();
    let io = build(&rows, 1024, 64).await;
    let key = parse_archive(&io).await.remove(&1).unwrap();
    assert_eq!(key.frames.len(), 2);

    let index = io.file(FileRole::Index).unwrap().bytes().await;
    let data = io.file(FileRole::Data(0)).unwrap().bytes().await;
    let cell = |minute: usize| key.l2_offset as usize + 8 + minute * 9;

    // Minute 3's cell pointing at minute 4's record.
    let mut moved = index.clone();
    moved[cell(3) + 5..cell(3) + 9].copy_from_slice(&(10u32 + 4 * 4_002).to_le_bytes());

    // The second frame chaining to itself.
    let mut looped = data.clone();
    looped[32_768 + 4..32_768 + 8].copy_from_slice(&32_768u32.to_le_bytes());

    for (case, index, data) in [
        ("a moved cell", moved, data.clone()),
        ("a frame chained to itself", index.clone(), looped),
    ] {
        let damaged = InMemoryIo::default();
        damaged.put(FileRole::Index, index);
        damaged.put(FileRole::Data(0), data);

        let reader = Reader::open(damaged, day()).await.unwrap();
        assert!(
            matches!(
                reader.get_day(&Login(1)).await,
                Err(ArchiveError::Corrupted(_))
            ),
            "{case}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writing_reading_and_compacting_run_inside_spawned_tasks() {
    let mut rng = seeded(3);
    let rows = random_rows(4, 200, &mut rng);
    let model = Model::of(&rows);
    let io = InMemoryIo::default();

    tokio::spawn({
        let io = io.clone();
        async move {
            let mut writer = Writer::new(io, NAME, day()).await.unwrap();
            write_all(&mut writer, &rows).await;
            writer.flush().await.unwrap();
            writer.finish().await.unwrap();
        }
    })
    .await
    .unwrap();

    let reader = Arc::new(open(&io).await);
    let login = *model.logins.keys().next().unwrap();

    let read = tokio::spawn({
        let reader = reader.clone();
        async move {
            reader
                .get_range(&Login(login), moment(0, 0), moment(1439, 0))
                .await
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert_eq!(read, model.range(login, time(0, 0), time(1439, 0)));

    let compacted = tokio::spawn(async move {
        let out = InMemoryIo::default();
        let plan = reader.compaction_plan().await.unwrap();
        reader.write_compacted_to(&plan, &out).await.unwrap();
        out
    })
    .await
    .unwrap();

    assert_reads_back(&open(&compacted).await, &model, "compacted in a task").await;
}

// The minutes a key holds records in, off its L2 frame: 10, 11, 20 and 1439 for key 5 — 10:59.999999 is in
// minute 10, and the two records at 11:00 are one minute. Key 9 is its own, and a key the archive does not
// hold has none.
#[tokio::test]
async fn the_minutes_a_key_has_records_in_are_read_off_its_cells() {
    let (io, _) = edges().await;
    let reader = open(&io).await;

    assert_eq!(
        reader.minutes_with_records(&Login(5)).await.unwrap(),
        vec![10, 11, 20, 1439]
    );
    assert_eq!(
        reader.minutes_with_records(&Login(9)).await.unwrap(),
        vec![15]
    );
    assert!(
        reader
            .minutes_with_records(&Login(77))
            .await
            .unwrap()
            .is_empty()
    );
}
