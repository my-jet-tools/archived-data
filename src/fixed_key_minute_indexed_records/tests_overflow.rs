//! The crate's one overflow rule, on data frames: a record after the last one of a full frame chains a new
//! frame; a record before a record of a full frame halves it, and the upper half moves into the next frame
//! when that has room, into a new frame chained between the two when it does not. Every cell follows the
//! records it points at.
//!
//! Records here are 4 000 bytes, 4 002 with their prefix, so a frame of 32 758 bytes of records holds 8 of
//! them with 742 free, and halves at exactly 4 records.

use super::test_support::{
    FRAME, Model, NAME, ParsedFrame, ParsedKey, Writer, assert_reads_back, build, compact, day,
    ids, open, parse_archive, record, seeded, time,
};
use crate::in_memory_io::InMemoryIo;
use crate::test_items::Login;

const LEN: usize = 4_000;
const WHOLE: u32 = 4_002;

/// Record `id` of the key at minute `minute`, `second` seconds in.
fn at(minute: i64, second: i64, id: u64) -> Vec<u8> {
    record(time(minute, second * 1_000_000), id, LEN)
}

/// Where record `index` of a frame starts: after `index` whole records.
fn nth(index: u32) -> u32 {
    10 + index * WHOLE
}

fn frame(offset: u32, next: u32, ids: &[u64]) -> ParsedFrame {
    ParsedFrame {
        file_no: 0,
        offset,
        size: FRAME as u32,
        next,
        ids: ids.to_vec(),
    }
}

/// A writer that keeps nothing cached between writes, so every frame is read back from its file.
async fn writer(io: &InMemoryIo) -> Writer {
    let mut writer = Writer::new(io.clone(), NAME, day()).await.unwrap();
    writer.set_l2_cache_size(0);
    writer.set_data_cache_size(0);
    writer
}

/// Writes `records` of key 1, finishes, and parses the archive back — checking every cell against the records
/// on the way — and every read against the model.
async fn written(records: &[Vec<u8>]) -> ParsedKey {
    let io = InMemoryIo::default();
    let mut writer = writer(&io).await;
    let mut model = Model::default();

    for record in records {
        writer.write(Login(1), record).await.unwrap();
        model.push(1, record);
    }

    writer.finish().await.unwrap();

    let reader = open(&io).await;
    assert_eq!(reader.get_day(&Login(1)).await.unwrap(), model.day(1));

    parse_archive(&io).await.remove(&1).unwrap()
}

/// Records 1 ..= `count`, record `n` at minute `n`, 10 seconds in, written in order.
fn in_order(count: u64) -> Vec<Vec<u8>> {
    (1..=count).map(|n| at(n as i64, 10, n)).collect()
}

#[tokio::test]
async fn a_record_after_the_last_of_a_full_frame_chains_a_new_frame_and_moves_nothing() {
    let key = written(&in_order(8)).await;
    assert_eq!(key.frames, [frame(0, 0, &[1, 2, 3, 4, 5, 6, 7, 8])]);

    let key = written(&in_order(9)).await;
    assert_eq!(
        key.frames,
        [
            frame(0, FRAME as u32, &[1, 2, 3, 4, 5, 6, 7, 8]),
            frame(FRAME as u32, 0, &[9])
        ]
    );

    for n in 1..=8 {
        assert_eq!(key.cells[n], Some((0, 0, nth(n as u32 - 1))), "minute {n}");
    }
    assert_eq!(key.cells[9], Some((0, FRAME as u32, nth(0))));

    // Into the same minute as the last record, after it: still the end of the chain.
    let mut records = in_order(8);
    records.push(at(8, 20, 9));
    let key = written(&records).await;
    assert_eq!(key.frames[1], frame(FRAME as u32, 0, &[9]));
    assert_eq!(
        key.cells[8],
        Some((0, 0, nth(7))),
        "the minute's first record stays"
    );
}

#[tokio::test]
async fn a_record_into_a_frame_with_room_moves_the_records_after_it_and_their_cells() {
    // 1, 2, 4, 5 at minutes 1, 2, 4, 5; then 3 at minute 3, and 6 before 2 in minute 2.
    let records = vec![
        at(1, 10, 1),
        at(2, 10, 2),
        at(4, 10, 4),
        at(5, 10, 5),
        at(3, 10, 3),
        at(2, 5, 6),
    ];
    let key = written(&records).await;

    assert_eq!(key.frames, [frame(0, 0, &[1, 6, 2, 3, 4, 5])]);
    assert_eq!(key.cells[1], Some((0, 0, nth(0))));
    assert_eq!(
        key.cells[2],
        Some((0, 0, nth(1))),
        "6 is now the first of minute 2"
    );
    assert_eq!(key.cells[3], Some((0, 0, nth(3))));
    assert_eq!(key.cells[4], Some((0, 0, nth(4))));
    assert_eq!(key.cells[5], Some((0, 0, nth(5))));
}

#[tokio::test]
async fn a_full_frame_halves_and_its_upper_half_moves_in_front_of_a_next_frame_with_room() {
    // Frame 0: 1 ..= 8; frame 1: 9. Record 10 goes between 2 and 3, in minute 2 after 2.
    let mut records = in_order(9);
    records.push(at(2, 30, 10));
    let key = written(&records).await;

    let second = FRAME as u32;
    assert_eq!(
        key.frames,
        [
            frame(0, second, &[1, 2, 10, 3, 4]),
            frame(second, 0, &[5, 6, 7, 8, 9])
        ]
    );

    assert_eq!(key.cells[1], Some((0, 0, nth(0))));
    assert_eq!(
        key.cells[2],
        Some((0, 0, nth(1))),
        "2 is still the first of its minute"
    );
    assert_eq!(key.cells[3], Some((0, 0, nth(3))), "3 moved up past 10");
    assert_eq!(key.cells[4], Some((0, 0, nth(4))));

    for (minute, index) in [(5, 0), (6, 1), (7, 2), (8, 3), (9, 4)] {
        assert_eq!(
            key.cells[minute],
            Some((0, second, nth(index))),
            "minute {minute} moved into the next frame"
        );
    }
}

#[tokio::test]
async fn a_record_of_the_upper_half_moves_with_it_and_becomes_the_first_of_its_minute() {
    // Record 10 goes before 6, in minute 6: into the upper half, which moves into frame 1.
    let mut records = in_order(9);
    records.push(at(6, 5, 10));
    let key = written(&records).await;

    let second = FRAME as u32;
    assert_eq!(
        key.frames,
        [
            frame(0, second, &[1, 2, 3, 4]),
            frame(second, 0, &[5, 10, 6, 7, 8, 9])
        ]
    );

    assert_eq!(key.cells[5], Some((0, second, nth(0))));
    assert_eq!(
        key.cells[6],
        Some((0, second, nth(1))),
        "10 is the first of minute 6"
    );
    assert_eq!(key.cells[7], Some((0, second, nth(3))));
    assert_eq!(key.cells[9], Some((0, second, nth(5))));
}

#[tokio::test]
async fn a_full_frame_whose_next_has_no_room_chains_a_new_frame_between_them() {
    // Frame 0: 1 ..= 8; frame 1: 9 ..= 13, 20 010 bytes, with 12 748 free — too few for the upper half.
    let mut records = in_order(13);
    records.push(at(2, 30, 20));
    let key = written(&records).await;

    let (second, third) = (FRAME as u32, 2 * FRAME as u32);
    assert_eq!(
        key.frames,
        [
            frame(0, third, &[1, 2, 20, 3, 4]),
            frame(third, second, &[5, 6, 7, 8]),
            frame(second, 0, &[9, 10, 11, 12, 13])
        ],
        "the new frame lies at the end of the file and points back at the next one"
    );

    for (minute, index) in [(5, 0), (6, 1), (7, 2), (8, 3)] {
        assert_eq!(
            key.cells[minute],
            Some((0, third, nth(index))),
            "minute {minute}"
        );
    }

    for (minute, index) in [(9, 0), (13, 4)] {
        assert_eq!(
            key.cells[minute],
            Some((0, second, nth(index))),
            "minute {minute}"
        );
    }
}

#[tokio::test]
async fn the_last_frame_halves_into_a_new_last_frame() {
    // One full frame and no next: a record before 8 halves it into a new frame chained after it.
    let mut records = in_order(8);
    records.push(at(8, 1, 9));
    let key = written(&records).await;

    let second = FRAME as u32;
    assert_eq!(
        key.frames,
        [
            frame(0, second, &[1, 2, 3, 4]),
            frame(second, 0, &[5, 6, 7, 9, 8])
        ]
    );
    assert_eq!(
        key.cells[8],
        Some((0, second, nth(3))),
        "9 is the first of minute 8"
    );
}

#[tokio::test]
async fn a_record_before_the_first_of_a_full_first_frame_stays_in_the_first_frame() {
    let mut records = in_order(8);
    records.push(at(0, 0, 9));
    let key = written(&records).await;

    assert_eq!(key.frames[0], frame(0, FRAME as u32, &[9, 1, 2, 3, 4]));
    assert_eq!(key.cells[0], Some((0, 0, nth(0))));
    assert_eq!(key.cells[1], Some((0, 0, nth(1))));
    assert_eq!(key.cells[5], Some((0, FRAME as u32, nth(0))));
}

#[tokio::test]
async fn an_empty_minute_takes_its_place_before_the_next_minute_with_a_record() {
    // Minutes 1 and 9 hold records, 5 does not: a record at 5 goes before 9's first, in 9's frame.
    let records = vec![
        at(1, 0, 1),
        at(9, 0, 2),
        at(5, 0, 3),
        at(7, 0, 4),
        at(3, 0, 5),
    ];
    let key = written(&records).await;

    assert_eq!(ids(&key.records), [1, 5, 3, 4, 2]);
    assert_eq!(key.cells[3], Some((0, 0, nth(1))));
    assert_eq!(key.cells[5], Some((0, 0, nth(2))));
    assert_eq!(key.cells[7], Some((0, 0, nth(3))));
    assert_eq!(key.cells[9], Some((0, 0, nth(4))));
}

#[tokio::test]
async fn an_empty_minute_past_every_record_goes_to_the_end_of_the_chain() {
    // Three frames; a record at a minute past all of them is appended to the last frame, found from the last
    // minute with a record.
    let mut records = in_order(17);
    records.push(at(1000, 0, 18));
    let key = written(&records).await;

    let last = key.frames.last().unwrap();
    assert_eq!(last.ids, [17, 18]);
    assert_eq!(key.cells[1000], Some((0, last.offset, nth(1))));
}

#[tokio::test]
async fn frames_halve_again_and_again_and_every_cell_keeps_following_its_record() {
    // Every record lands before the one written before it: each insert goes into a full frame's front.
    let records: Vec<Vec<u8>> = (0..60u64)
        .map(|n| at(1_000 - n as i64 * 3, 10, n))
        .collect();

    let key = written(&records).await;
    assert_eq!(key.records.len(), 60);
    assert!(key.frames.len() >= 8, "{} frames", key.frames.len());

    let expected: Vec<u64> = (0..60).rev().collect();
    assert_eq!(ids(&key.records), expected);
}

#[tokio::test]
async fn records_of_any_size_crowded_into_a_few_minutes_split_every_way_and_read_back() {
    // Two keys, four minutes, three distinct microseconds, 8 bytes to 8 KiB: every overflow branch is taken
    // many times over, and every cell is checked against the bytes after each archive.
    for seed in 0..30u64 {
        let mut rng = seeded(seed + 1_000);
        let count = 20 + (rng.next() % 200) as usize;

        let rows: Vec<(u64, Vec<u8>)> = (0..count as u64)
            .map(|id| {
                let at = time((rng.next() % 4) as i64, (rng.next() % 3) as i64);
                let len = 8 + (rng.next() % 8_185) as usize;
                (1 + rng.next() % 2, record(at, id, len))
            })
            .collect();

        let model = Model::of(&rows);
        let io = build(&rows, 1, (rng.next() % 3) as usize).await;

        for (login, key) in parse_archive(&io).await {
            assert!(
                key.records == model.day(login),
                "seed {seed}, login {login}"
            );
        }

        let compacted = compact(&open(&io).await).await;
        assert_reads_back(&open(&compacted).await, &model, &format!("seed {seed}")).await;
    }
}
