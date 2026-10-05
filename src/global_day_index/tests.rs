use rust_extensions::date_time::DateTimeAsMicroseconds;

use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader};
use crate::test_items::Login;
use crate::{
    ArchiveError, GlobalDayIndex, GlobalDayIndexReader, IoReaderFactory, MICROSECONDS_PER_DAY,
    days_from_civil, l0_hash,
};

type Index = GlobalDayIndex<Login>;
type Reader = GlobalDayIndexReader<Login, InMemoryReader>;

/// The first microsecond of `year-month-day`.
fn day(year: i64, month: u32, day: u32) -> DateTimeAsMicroseconds {
    DateTimeAsMicroseconds::new(days_from_civil(year, month, day) * MICROSECONDS_PER_DAY)
}

fn at(year: i64, month: u32, day_of_month: u32, hour: i64) -> DateTimeAsMicroseconds {
    let start = day(year, month, day_of_month).unix_microseconds;
    DateTimeAsMicroseconds::new(start + hour * 3_600_000_000)
}

fn starts(days: &[DateTimeAsMicroseconds]) -> Vec<i64> {
    days.iter().map(|day| day.unix_microseconds).collect()
}

fn dates(days: &[(i64, u32, u32)]) -> Vec<i64> {
    days.iter()
        .map(|(year, month, dom)| day(*year, *month, *dom).unix_microseconds)
        .collect()
}

async fn reader_of(bytes: Vec<u8>) -> (Reader, InMemoryIo) {
    let io = InMemoryIo::default();
    io.put(FileRole::Index, bytes);
    let reader = Reader::open(io.create_index_reader().await.unwrap())
        .await
        .unwrap();
    (reader, io)
}

/// The file of one login, `7`, with 1 January, 29 February and 1 March 2024 and 1 March 2025, from the
/// written-down layout with its literal sizes and none of the crate's code.
fn reference() -> Vec<u8> {
    const L0: usize = 20 + 20;
    const L1: usize = 12 + 1024 * 14;

    let mut out = 1_789_000_000_123_456i64.to_le_bytes().to_vec();

    out.extend_from_slice(&(L0 as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&l0_hash(L0 as u32, 0, 1).to_le_bytes());
    out.extend_from_slice(&7u64.to_le_bytes());
    out.extend_from_slice(&7u64.to_le_bytes());
    out.extend_from_slice(&((8 + L0) as u32).to_le_bytes());

    let l1 = out.len();
    out.extend_from_slice(&(L1 as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&7u64.to_le_bytes());
    out.extend_from_slice(&((8 + L0 + L1) as u32).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.resize(l1 + L1, 0);

    // 2024: day 0 is bit 0x01 of byte 0; day 59 is 0x08 of byte 7; day 60 is 0x10 of byte 7.
    let mut year = [0u8; 48];
    year[..2].copy_from_slice(&2024u16.to_le_bytes());
    year[2] = 0x01;
    year[2 + 7] = 0x08 | 0x10;
    out.extend_from_slice(&year);

    // 2025: 1 March is day 60 in every year.
    let mut year = [0u8; 48];
    year[..2].copy_from_slice(&2025u16.to_le_bytes());
    year[2 + 7] = 0x10;
    out.extend_from_slice(&year);

    out
}

#[tokio::test]
async fn the_file_is_exactly_the_written_down_layout() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(1_789_000_000_123_456));

    // Out of order, a day twice, a moment inside a day rather than its start.
    index.set_day(Login(7), at(2025, 3, 1, 23)).unwrap();
    index.set_day(Login(7), day(2024, 2, 29)).unwrap();
    index.set_day(Login(7), day(2024, 1, 1)).unwrap();
    index.set_day(Login(7), at(2024, 3, 1, 12)).unwrap();
    index.set_day(Login(7), day(2024, 1, 1)).unwrap();

    let bytes = index.to_bytes().unwrap();
    assert!(bytes == reference());

    let parsed = Index::from_bytes(bytes).await.unwrap();
    assert_eq!(parsed, index);
}

#[tokio::test]
async fn february_29_and_march_1_have_the_same_bits_in_every_year() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(0));

    for year in [2023, 2024, 2100, 2000] {
        index.set_day(Login(1), day(year, 2, 28)).unwrap();
        index.set_day(Login(1), day(year, 3, 1)).unwrap();
    }
    index.set_day(Login(1), day(2024, 2, 29)).unwrap();
    index.set_day(Login(1), day(2000, 2, 29)).unwrap();

    let bytes = index.to_bytes().unwrap();
    let data = bytes.len() - 4 * 48;

    for (block, (year, leap)) in [(2000, true), (2023, false), (2024, true), (2100, false)]
        .into_iter()
        .enumerate()
    {
        let block = &bytes[data + block * 48..data + (block + 1) * 48];
        assert_eq!(
            u16::from_le_bytes([block[0], block[1]]),
            year,
            "years ascend"
        );

        // 28 February is day 58: bit 0x04 of byte 7. 29 February 0x08, 1 March 0x10.
        let expected = if leap {
            0x04 | 0x08 | 0x10
        } else {
            0x04 | 0x10
        };
        assert_eq!(block[2 + 7], expected, "{year}");
        assert!(
            block[2..]
                .iter()
                .enumerate()
                .all(|(at, byte)| at == 7 || *byte == 0)
        );
    }

    let (reader, _) = reader_of(bytes).await;

    for (year, leap) in [(2023, false), (2024, true), (2100, false), (2000, true)] {
        let mut expected = vec![(year, 2, 28)];
        if leap {
            expected.push((year, 2, 29));
        }
        expected.push((year, 3, 1));

        let found = reader
            .days_with_data(&Login(1), day(year, 1, 1), day(year, 12, 31))
            .await
            .unwrap();
        assert_eq!(starts(&found), dates(&expected), "{year}");
    }
}

#[tokio::test]
async fn a_new_version_adds_logins_and_years_to_the_old_one() {
    let mut old = Index::new(DateTimeAsMicroseconds::new(100));
    old.set_day(Login(20), day(2025, 12, 31)).unwrap();
    old.set_day(Login(10), day(2026, 1, 1)).unwrap();
    let old_bytes = old.to_bytes().unwrap();

    let mut new = Index::from_bytes(old_bytes.clone()).await.unwrap();
    assert_eq!(new.sync_time().unix_microseconds, 100);

    new.set_sync_time(DateTimeAsMicroseconds::new(200));
    for login in [30, 10, 5] {
        new.set_day(Login(login), day(2026, 9, 16)).unwrap();
    }
    new.set_day(Login(20), day(2027, 1, 1)).unwrap();
    new.set_day(Login(20), day(2024, 6, 1)).unwrap();

    let bytes = new.to_bytes().unwrap();
    let (reader, _) = reader_of(bytes.clone()).await;

    assert_eq!(reader.sync_time().unix_microseconds, 200);
    assert_eq!(reader.logins_count().await.unwrap(), 4);
    assert_eq!(new.logins_count(), 4);

    let everything = (
        DateTimeAsMicroseconds::new(i64::MIN),
        DateTimeAsMicroseconds::new(i64::MAX),
    );

    for (login, expected) in [
        (5, vec![(2026, 9, 16)]),
        (10, vec![(2026, 1, 1), (2026, 9, 16)]),
        (20, vec![(2024, 6, 1), (2025, 12, 31), (2027, 1, 1)]),
        (30, vec![(2026, 9, 16)]),
    ] {
        let found = reader
            .days_with_data(&Login(login), everything.0, everything.1)
            .await
            .unwrap();
        assert_eq!(starts(&found), dates(&expected), "login {login}");
        assert_eq!(
            starts(&new.days_with_data(&Login(login), everything.0, everything.1)),
            dates(&expected)
        );
    }

    // The old version is untouched by the new one.
    let old = Index::from_bytes(old_bytes).await.unwrap();
    assert_eq!(old.logins_count(), 2);
}

#[tokio::test]
async fn setting_a_day_again_changes_nothing() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(5));

    for login in 0..3000 {
        index.set_day(Login(login * 3), day(2026, 9, 15)).unwrap();
    }
    let once = index.to_bytes().unwrap();

    for login in 0..3000 {
        index.set_day(Login(login * 3), at(2026, 9, 15, 7)).unwrap();
    }
    assert!(index.to_bytes().unwrap() == once);

    let mut rebuilt = Index::from_bytes(once.clone()).await.unwrap();
    for login in (0..3000).rev() {
        rebuilt.set_day(Login(login * 3), day(2026, 9, 15)).unwrap();
    }
    assert!(
        rebuilt.to_bytes().unwrap() == once,
        "a retry of the same day"
    );
}

#[tokio::test]
async fn days_with_data_over_years_holds_both_ends_and_nothing_outside() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(0));
    let set = [
        (2023, 12, 31),
        (2024, 1, 1),
        (2024, 2, 29),
        (2024, 12, 31),
        (2025, 7, 4),
        (2026, 1, 1),
        (2026, 9, 15),
    ];

    for (year, month, dom) in set {
        index.set_day(Login(42), day(year, month, dom)).unwrap();
    }

    let (reader, io) = reader_of(index.to_bytes().unwrap()).await;

    for (from, to, expected) in [
        (at(2023, 12, 31, 23), at(2026, 1, 1, 0), set[..6].to_vec()),
        (at(2024, 1, 1, 12), at(2025, 7, 4, 0), set[1..5].to_vec()),
        (day(2024, 3, 1), day(2025, 7, 3), vec![(2024, 12, 31)]),
        (day(2024, 3, 1), day(2024, 12, 30), vec![]),
        (day(2026, 9, 16), day(2030, 1, 1), vec![]),
        (day(2026, 9, 15), day(2026, 9, 15), vec![(2026, 9, 15)]),
        (day(2025, 1, 1), day(2024, 1, 1), vec![]),
    ] {
        let expected = dates(&expected);
        let found = reader.days_with_data(&Login(42), from, to).await.unwrap();

        assert_eq!(starts(&found), expected, "{from:?} ..= {to:?}");
        assert_eq!(
            starts(&index.days_with_data(&Login(42), from, to)),
            expected
        );
    }

    // After the first lookup the L1 page is cached: a lookup is one read of the login's blocks.
    let file = io.file(FileRole::Index).unwrap();
    let before = file.read_ats();
    reader
        .days_with_data(&Login(42), day(2020, 1, 1), day(2030, 1, 1))
        .await
        .unwrap();
    assert_eq!(file.read_ats(), before + 1);
}

#[tokio::test]
async fn a_login_the_index_does_not_hold_has_no_day_and_costs_no_block_read() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(0));
    for login in [10, 20, 30] {
        index.set_day(Login(login), day(2026, 9, 15)).unwrap();
    }

    let (reader, io) = reader_of(index.to_bytes().unwrap()).await;
    let range = (day(2000, 1, 1), day(2100, 1, 1));

    for login in [0, 15, 31, u64::MAX] {
        assert!(
            reader
                .days_with_data(&Login(login), range.0, range.1)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            index
                .days_with_data(&Login(login), range.0, range.1)
                .is_empty()
        );
    }

    assert_eq!(io.file(FileRole::Index).unwrap().read_ats(), 3 + 1);

    let empty = Index::new(DateTimeAsMicroseconds::new(9))
        .to_bytes()
        .unwrap();
    assert_eq!(empty.len(), 8 + 20);
    let (reader, _) = reader_of(empty).await;
    assert!(
        reader
            .days_with_data(&Login(1), range.0, range.1)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(reader.logins_count().await.unwrap(), 0);
}

#[tokio::test]
async fn the_sync_time_round_trips_and_is_read_alone_with_one_read() {
    for sync_time in [0, -1, 1_789_517_696_123_456, i64::MIN, i64::MAX] {
        let mut index = Index::new(DateTimeAsMicroseconds::new(sync_time));
        index.set_day(Login(1), day(2026, 1, 1)).unwrap();
        let bytes = index.to_bytes().unwrap();

        assert_eq!(&bytes[..8], &sync_time.to_le_bytes());
        assert_eq!(
            Index::sync_time_of(&bytes[..8]).unwrap().unix_microseconds,
            sync_time
        );
        assert_eq!(
            Index::from_bytes(bytes.clone())
                .await
                .unwrap()
                .sync_time()
                .unix_microseconds,
            sync_time
        );

        let io = InMemoryIo::default();
        let file = io.put(FileRole::Index, bytes);
        let raw = io.create_index_reader().await.unwrap();
        assert_eq!(
            Reader::read_sync_time(&raw)
                .await
                .unwrap()
                .unix_microseconds,
            sync_time
        );
        assert_eq!((file.read_ats(), file.bytes_read()), (1, 8));
    }

    assert!(matches!(
        Index::sync_time_of(&[1, 2, 3]),
        Err(ArchiveError::Corrupted(_))
    ));
}

#[tokio::test]
async fn more_logins_than_a_page_holds_take_more_pages() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(0));

    for login in 0..2_500u64 {
        index
            .set_day(
                Login(login),
                day(2020 + (login % 7) as i64, 5, 1 + (login % 28) as u32),
            )
            .unwrap();
    }

    let bytes = index.to_bytes().unwrap();
    assert_eq!(
        u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        3,
        "three L1 pages"
    );

    let (reader, _) = reader_of(bytes.clone()).await;
    assert_eq!(reader.logins_count().await.unwrap(), 2_500);

    for login in [0u64, 1_023, 1_024, 2_047, 2_048, 2_499] {
        let expected = day(2020 + (login % 7) as i64, 5, 1 + (login % 28) as u32);
        let found = reader
            .days_with_data(&Login(login), day(2000, 1, 1), day(2100, 1, 1))
            .await
            .unwrap();
        assert_eq!(found, [expected], "login {login}");
    }

    assert_eq!(Index::from_bytes(bytes).await.unwrap(), index);
}

#[tokio::test]
async fn bytes_that_are_not_an_index_or_an_index_cut_short_are_corrupted() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(77));
    index.set_day(Login(3), day(2026, 1, 1)).unwrap();
    index.set_day(Login(4), day(2026, 1, 2)).unwrap();
    let bytes = index.to_bytes().unwrap();

    let mut common_leap_day = bytes.clone();
    let last_block = bytes.len() - 48;
    common_leap_day[last_block + 2 + 7] |= 0x08;

    let mut out_of_order = bytes.clone();
    out_of_order[8 + 40 + 12 + 14..8 + 40 + 12 + 22].copy_from_slice(&2u64.to_le_bytes());

    for (case, bytes) in [
        ("empty", vec![]),
        ("a sync time alone", bytes[..8].to_vec()),
        ("a split archive index", bytes[8..].to_vec()),
        ("cut inside the L1 page", bytes[..8 + 40 + 100].to_vec()),
        ("cut inside the blocks", bytes[..bytes.len() - 1].to_vec()),
        ("29 February 2026", common_leap_day),
        ("logins out of order", out_of_order),
    ] {
        assert!(
            matches!(
                Index::from_bytes(bytes).await,
                Err(ArchiveError::Corrupted(_))
            ),
            "{case}"
        );
    }
}

#[test]
fn a_day_in_a_year_a_block_cannot_hold_is_refused_and_changes_nothing() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(0));

    for moment in [day(-1, 12, 31), day(65_536, 1, 1)] {
        assert!(matches!(
            index.set_day(Login(1), moment),
            Err(ArchiveError::ContractViolation(_))
        ));
    }

    assert_eq!(index.logins_count(), 0);
    index.set_day(Login(1), day(0, 1, 1)).unwrap();
    index.set_day(Login(1), day(65_535, 12, 31)).unwrap();
    assert_eq!(
        index
            .days_with_data(&Login(1), day(-5, 1, 1), day(70_000, 1, 1))
            .len(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_is_parsed_and_read_inside_spawned_tasks() {
    let mut index = Index::new(DateTimeAsMicroseconds::new(1));
    index.set_day(Login(8), day(2026, 9, 17)).unwrap();
    let bytes = index.to_bytes().unwrap();

    let parsed = tokio::spawn({
        let bytes = bytes.clone();
        async move { Index::from_bytes(bytes).await.unwrap() }
    })
    .await
    .unwrap();
    assert_eq!(parsed, index);

    let (reader, _) = reader_of(bytes).await;
    let days = tokio::spawn(async move {
        reader
            .days_with_data(&Login(8), day(2026, 1, 1), day(2026, 12, 31))
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(days, [day(2026, 9, 17)]);
}
