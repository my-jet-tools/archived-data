use std::collections::BTreeMap;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

use super::test_types::{Hour, Login, Record, Rng, reference_day};
use crate::{
    ArchiveError, FixedKeyPreallocatedSlotsArchiveReaderSingleFile,
    FixedKeyPreallocatedSlotsArchiveWriterSingleFile,
};

type Writer<T> = FixedKeyPreallocatedSlotsArchiveWriterSingleFile<Login, Hour, Record, T>;
type Reader<T> = FixedKeyPreallocatedSlotsArchiveReaderSingleFile<Login, Hour, Record, T>;

type Day = BTreeMap<u64, Vec<Option<Record>>>;

/// `logins` × up to three distinct hours each, as `(login, hour, record)` fully shuffled. Each slot is
/// written once, so every order of the rows describes the same day.
fn rows(logins: &[u64], rng: &mut Rng) -> (Vec<(u64, usize, Record)>, Day) {
    let mut rows = Vec::new();
    let mut day: Day = BTreeMap::new();

    for login in logins {
        let hours = day.entry(*login).or_insert_with(|| vec![None; 24]);

        for _ in 0..3 {
            let hour = (rng.next() % 24) as usize;

            if hours[hour].is_some() {
                continue;
            }

            let record = Record::with(login * 100 + hour as u64 + rng.next() % 7);
            hours[hour] = Some(record.clone());
            rows.push((*login, hour, record));
        }
    }

    rng.shuffle(&mut rows);
    (rows, day)
}

async fn write_all<T>(writer: &mut Writer<T>, rows: &[(u64, usize, Record)])
where
    T: AsyncRead + tokio::io::AsyncWrite + AsyncSeek + Unpin,
{
    for (login, hour, record) in rows {
        writer
            .write(Login(*login), Hour(*hour), record)
            .await
            .unwrap();
    }
}

async fn build(
    rows: &[(u64, usize, Record)],
    capacity_l0: usize,
    capacity_l1: usize,
    cache: usize,
) -> Vec<u8> {
    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), capacity_l0, capacity_l1);
    writer.set_l1_cache_size(cache);
    writer.set_data_cache_size(cache);
    write_all(&mut writer, rows).await;
    writer.finish().await.unwrap().into_inner()
}

async fn assert_reads_back<T: AsyncRead + AsyncSeek + Unpin>(reader: &Reader<T>, day: &Day) {
    for (login, hours) in day {
        let items = reader
            .get(&Login(*login), Hour(0), Hour(23))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("login {login} is absent"));

        let expected: Vec<Record> = hours
            .iter()
            .map(|hour| hour.clone().unwrap_or_default())
            .collect();
        assert_eq!(items, expected, "login {login}");
    }
}

fn logins(count: u64, rng: &mut Rng) -> Vec<u64> {
    let mut logins: Vec<u64> = (0..count).map(|i| i * 3 + 1_000 + rng.next() % 2).collect();
    logins.sort_unstable();
    logins.dedup();
    logins
}

#[tokio::test]
async fn random_insert_orders_with_splits_read_back() {
    for seed in [1u64, 7, 42, 2026] {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let logins = logins(300, &mut rng);
        let (rows, day) = rows(&logins, &mut rng);

        // Capacity 4 splits constantly; cache 2 forces evictions and reloads of both page kinds.
        let bytes = build(&rows, 1024, 4, 2).await;
        let reader = Reader::open(Cursor::new(bytes)).await.unwrap();

        assert!(reader.l0_intervals() > 1, "the test must split");
        assert_eq!(reader.keys_count().await.unwrap(), day.len());
        assert_reads_back(&reader, &day).await;
    }
}

#[tokio::test]
async fn an_l1_capacity_of_one_splits_into_one_key_per_page() {
    let mut rng = Rng(99);
    let (rows, day) = rows(&[5, 1, 9, 3, 7], &mut rng);

    let bytes = build(&rows, 8, 1, 1).await;
    let reader = Reader::open(Cursor::new(bytes)).await.unwrap();

    assert_eq!(reader.l0_intervals(), 5);
    assert_reads_back(&reader, &day).await;
}

#[tokio::test]
async fn the_writer_reports_its_keys_and_intervals() {
    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 16, 2);

    for (login, value) in [(10, 1), (20, 2), (30, 3), (20, 4), (10, 5)] {
        writer
            .write(Login(login), Hour(1), &Record::with(value))
            .await
            .unwrap();
    }

    assert_eq!(writer.keys_count(), 3);
    assert_eq!(writer.l0_intervals(), 2);

    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();
    let got = reader.get(&Login(10), Hour(1), Hour(1)).await.unwrap();
    assert_eq!(
        got,
        Some(vec![Record::with(5)]),
        "a known key is overwritten in place"
    );
}

fn le_u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The offsets of every L0 frame, walked straight off the bytes.
fn frame_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = vec![0usize];

    loop {
        let next = le_u64_at(bytes, offsets[offsets.len() - 1] + 4) as usize;
        if next == 0 {
            return offsets;
        }
        offsets.push(next);
    }
}

#[tokio::test]
async fn l0_chains_across_several_frames_and_reads_back() {
    for seed in [3u64, 11, 500] {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let logins = logins(400, &mut rng);
        let (rows, day) = rows(&logins, &mut rng);

        // Two intervals per frame and two keys per L1 page: over a hundred frames.
        let bytes = build(&rows, 2, 2, 2).await;
        let reader = Reader::open(Cursor::new(bytes.clone())).await.unwrap();

        assert!(reader.l0_frames() > 3, "the test must chain");
        assert_eq!(reader.l0_frames(), reader.l0_intervals().div_ceil(2));
        assert_eq!(frame_offsets(&bytes).len(), reader.l0_frames());
        assert_eq!(reader.keys_count().await.unwrap(), day.len());
        assert_reads_back(&reader, &day).await;

        let mut compact = Vec::new();
        reader.write_compacted_to(&mut compact).await.unwrap();
        assert!(
            compact == reference_day(&day),
            "a chain compacts to one frame"
        );
        assert_eq!(
            Reader::open(Cursor::new(compact))
                .await
                .unwrap()
                .l0_frames(),
            1
        );
    }
}

#[tokio::test]
async fn continuations_are_reused_by_every_later_flush() {
    let mut rng = Rng(77);
    let logins = logins(300, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 3, 2);
    writer.set_data_cache_size(4);

    let mut frames = 0;
    for chunk in rows.chunks(rows.len() / 5 + 1) {
        write_all(&mut writer, chunk).await;
        writer.flush().await.unwrap();

        assert!(writer.l0_frames() >= frames, "a frame is never given back");
        assert_eq!(writer.l0_frames(), writer.l0_intervals().div_ceil(3));
        frames = writer.l0_frames();
    }

    let bytes = writer.finish().await.unwrap().into_inner();
    let offsets = frame_offsets(&bytes);
    assert_eq!(offsets.len(), frames);
    assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));

    let reader = Reader::open(Cursor::new(bytes)).await.unwrap();
    assert_reads_back(&reader, &day).await;
}

#[tokio::test]
async fn a_corrupted_l0_chain_is_refused() {
    let mut rng = Rng(5150);
    let logins = logins(120, &mut rng);
    let (rows, _) = rows(&logins, &mut rng);
    let bytes = build(&rows, 2, 2, 2).await;

    let offsets = frame_offsets(&bytes);
    assert!(offsets.len() >= 3, "the test needs three frames");

    let with_next = |frame: usize, next: u64| {
        let mut bytes = bytes.clone();
        let at = offsets[frame] + 4;
        bytes[at..at + 8].copy_from_slice(&next.to_le_bytes());
        bytes
    };

    let cases = [
        ("into frame 0", with_next(0, 8)),
        ("to itself", with_next(1, offsets[1] as u64)),
        ("back to an earlier frame", with_next(2, offsets[1] as u64)),
        ("into frame 0 from a continuation", with_next(2, 0x10)),
        ("past the end", with_next(1, bytes.len() as u64 + 4096)),
    ];

    for (case, bytes) in cases {
        assert!(
            matches!(
                Reader::open(Cursor::new(bytes)).await,
                Err(ArchiveError::Corrupted(_))
            ),
            "a next {case} was accepted"
        );
    }

    // Intervals out of order ACROSS frames: the first `from` of frame 1 below everything in frame 0.
    let mut unordered = bytes.clone();
    let at = offsets[1] + 16;
    unordered[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
    assert!(matches!(
        Reader::open(Cursor::new(unordered)).await,
        Err(ArchiveError::Corrupted(_))
    ));
}

#[tokio::test]
async fn a_slot_out_of_range_is_refused_on_write_and_read() {
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    let result = writer.write(Login(1), Hour(24), &Record::default()).await;
    assert!(matches!(result, Err(ArchiveError::ContractViolation(_))));

    writer
        .write(Login(1), Hour(0), &Record::with(1))
        .await
        .unwrap();
    let bytes = writer.finish().await.unwrap().into_inner();
    let reader = Reader::open(Cursor::new(bytes)).await.unwrap();

    assert!(matches!(
        reader.get(&Login(1), Hour(0), Hour(24)).await,
        Err(ArchiveError::ContractViolation(_))
    ));
}

#[tokio::test]
async fn get_returns_the_inclusive_range_in_slot_order() {
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    for hour in [3, 4, 6] {
        writer
            .write(Login(77), Hour(hour), &Record::with(hour as u64))
            .await
            .unwrap();
    }
    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();

    let items = reader
        .get(&Login(77), Hour(3), Hour(6))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        items,
        vec![
            Record::with(3),
            Record::with(4),
            Record::default(),
            Record::with(6)
        ]
    );

    let one = reader
        .get(&Login(77), Hour(4), Hour(4))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(one, vec![Record::with(4)]);
}

#[tokio::test]
async fn from_after_to_is_empty_without_reading_and_an_absent_key_is_none() {
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    for login in [10, 20, 30] {
        writer
            .write(Login(login), Hour(0), &Record::with(login))
            .await
            .unwrap();
    }
    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();

    assert_eq!(
        reader.get(&Login(20), Hour(5), Hour(4)).await.unwrap(),
        Some(vec![])
    );
    assert_eq!(
        reader.get(&Login(999), Hour(5), Hour(4)).await.unwrap(),
        Some(vec![])
    );
    assert!(
        reader.l1_cache.pages.read().await.is_empty(),
        "nothing was read"
    );

    for login in [0, 15, 31, 999] {
        assert_eq!(
            reader.get(&Login(login), Hour(0), Hour(23)).await.unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn an_empty_file_is_a_sixteen_byte_compact_and_answers_none() {
    let bytes = Writer::new(Cursor::new(Vec::new()))
        .finish()
        .await
        .unwrap()
        .into_inner();
    let reader = Reader::open(Cursor::new(bytes)).await.unwrap();

    assert_eq!(reader.keys_count().await.unwrap(), 0);
    assert_eq!(reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap(), None);
    assert_eq!(reader.compacted_len().await.unwrap(), 16);

    let mut out = Vec::new();
    reader.write_compacted_to(&mut out).await.unwrap();
    assert_eq!(out, reference_day(&BTreeMap::new()));
}

#[tokio::test]
async fn a_truncated_or_garbage_file_is_an_error_not_an_absent_key() {
    assert!(matches!(
        Reader::open(Cursor::new(vec![1, 2, 3])).await,
        Err(ArchiveError::Corrupted(_))
    ));

    let mut writer = Writer::new(Cursor::new(Vec::new()));
    writer
        .write(Login(5), Hour(0), &Record::with(5))
        .await
        .unwrap();
    let bytes = writer.finish().await.unwrap().into_inner();

    let cut = bytes[..bytes.len() - 100].to_vec();
    let reader = Reader::open(Cursor::new(cut)).await.unwrap();
    assert!(reader.get(&Login(5), Hour(0), Hour(0)).await.is_err());
}

#[tokio::test]
async fn compact_output_is_byte_identical_to_the_reference_whatever_the_insert_order() {
    let mut rng = Rng(0xDEAD_BEEF);
    // Past one compact L1 page (1024), with a partial last page.
    let logins = logins(2_500, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);
    let reference = reference_day(&day);

    let mut reversed = rows.clone();
    reversed.reverse();

    for (rows, capacity_l1, cache) in [(&rows, 1024, 64), (&reversed, 16, 3), (&rows, 7, 1)] {
        let bytes = build(rows, 1024, capacity_l1, cache).await;
        let reader = Reader::open(Cursor::new(bytes)).await.unwrap();

        assert_eq!(
            reader.compacted_len().await.unwrap(),
            reference.len() as u64
        );

        let mut out = Vec::new();
        let written = reader.write_compacted_to(&mut out).await.unwrap();
        assert_eq!(written, reference.len() as u64);
        assert!(
            out == reference,
            "compact bytes differ (capacity_l1 {capacity_l1})"
        );

        // A compact file is itself a readable file, and compacts to itself.
        let compact = Reader::open(Cursor::new(out.clone())).await.unwrap();
        assert_eq!(compact.l0_intervals(), 3);
        assert_eq!(compact.keys_count().await.unwrap(), day.len());
        assert_reads_back(&compact, &day).await;

        let mut again = Vec::new();
        compact.write_compacted_to(&mut again).await.unwrap();
        assert!(again == out);
    }
}

#[tokio::test]
async fn a_hit_stamps_last_access_and_a_full_cache_evicts_the_oldest() {
    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 16, 2);
    for login in [1, 2, 3, 4, 5, 6] {
        writer
            .write(Login(login), Hour(0), &Record::with(login))
            .await
            .unwrap();
    }
    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();
    reader.set_data_cache_size(2);

    let offset = |login| {
        let reader = &reader;
        async move { reader.data_offset_of(&Login(login)).await.unwrap().unwrap() }
    };
    let (a, b, c) = (offset(1).await, offset(2).await, offset(3).await);

    let stamp = |at: u64| {
        let reader = &reader;
        async move {
            reader.data_cache.pages.read().await[&at]
                .last_access
                .get_unix_microseconds()
        }
    };

    reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap();
    let first = stamp(a).await;
    tokio::time::sleep(Duration::from_millis(3)).await;

    reader.get(&Login(2), Hour(0), Hour(0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;

    reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap();
    assert!(stamp(a).await > first, "a hit moves last_access");
    tokio::time::sleep(Duration::from_millis(3)).await;

    reader.get(&Login(3), Hour(0), Hour(0)).await.unwrap();

    let cached: Vec<u64> = {
        let mut keys: Vec<u64> = reader
            .data_cache
            .pages
            .read()
            .await
            .keys()
            .copied()
            .collect();
        keys.sort_unstable();
        keys
    };
    let mut expected = vec![a, c];
    expected.sort_unstable();
    assert_eq!(cached, expected, "login 2 was the oldest and went");
    assert!(!cached.contains(&b));

    reader.set_l1_cache_size(1);
    reader.get(&Login(6), Hour(0), Hour(0)).await.unwrap();
    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);
}

/// A source whose reads wait for [`Gate::open`] while it is shut.
struct GatedSource {
    inner: Cursor<Vec<u8>>,
    gate: Arc<Gate>,
}

#[derive(Default)]
struct Gate {
    shut: AtomicBool,
    waiting: AtomicBool,
    waker: std::sync::Mutex<Option<Waker>>,
}

impl Gate {
    fn open(&self) {
        self.shut.store(false, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

impl AsyncRead for GatedSource {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.gate.shut.load(Ordering::SeqCst) {
            *self.gate.waker.lock().unwrap() = Some(cx.waker().clone());
            self.gate.waiting.store(true, Ordering::SeqCst);
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncSeek for GatedSource {
    fn start_seek(mut self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(&mut self.inner).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(&mut self.inner).poll_complete(cx)
    }
}

#[tokio::test]
async fn a_miss_reads_the_source_without_holding_a_cache_lock() {
    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 16, 2);
    for login in [1, 2, 3, 4] {
        writer
            .write(Login(login), Hour(0), &Record::with(login))
            .await
            .unwrap();
    }
    let bytes = writer.finish().await.unwrap().into_inner();

    let gate = Arc::new(Gate::default());
    let reader = Arc::new(
        Reader::open(GatedSource {
            inner: Cursor::new(bytes),
            gate: gate.clone(),
        })
        .await
        .unwrap(),
    );

    reader.get(&Login(1), Hour(0), Hour(0)).await.unwrap();

    gate.shut.store(true, Ordering::SeqCst);
    let missing = tokio::spawn({
        let reader = reader.clone();
        async move { reader.get(&Login(4), Hour(0), Hour(0)).await.unwrap() }
    });

    while !gate.waiting.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }

    // The miss is parked inside the source read; both caches must still be free to read AND write.
    let hit = tokio::time::timeout(
        Duration::from_secs(2),
        reader.get(&Login(1), Hour(0), Hour(0)),
    )
    .await
    .expect("a cached read waited on a source read");
    assert_eq!(hit.unwrap().unwrap(), vec![Record::with(1)]);

    tokio::time::timeout(Duration::from_secs(2), async {
        drop(reader.l1_cache.pages.write().await);
        drop(reader.data_cache.pages.write().await);
    })
    .await
    .expect("a cache lock was held across a source read");

    gate.open();
    assert_eq!(missing.await.unwrap().unwrap(), vec![Record::with(4)]);
}

#[tokio::test]
async fn through_tokio_files_and_an_arc_shared_compaction_task() {
    let dir = std::env::temp_dir().join(format!("archived-data-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let working = dir.join("day.temp");

    let mut rng = Rng(314);
    let logins = logins(1_500, &mut rng);
    let (rows, day) = rows(&logins, &mut rng);

    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&working)
        .await
        .unwrap();

    let mut writer = Writer::new_with_capacity(file, 1024, 64);
    writer.set_l1_cache_size(4);
    writer.set_data_cache_size(100);
    write_all(&mut writer, &rows).await;
    writer.finish().await.unwrap().sync_all().await.unwrap();

    let reader = Arc::new(
        Reader::open(tokio::fs::File::open(&working).await.unwrap())
            .await
            .unwrap(),
    );
    assert_reads_back(&reader, &day).await;

    // What the S3 upload producer does: a spawned ('static + Send) task over the shared reader.
    let compact = tokio::spawn({
        let reader = reader.clone();
        async move {
            let mut out = Vec::new();
            reader.write_compacted_to(&mut out).await?;
            Ok::<_, std::io::Error>(out)
        }
    })
    .await
    .unwrap()
    .unwrap();

    assert!(compact == reference_day(&day));

    let compact_path = dir.join("day");
    std::fs::write(&compact_path, &compact).unwrap();
    let from_file = Reader::open(tokio::fs::File::open(&compact_path).await.unwrap())
        .await
        .unwrap();
    assert_reads_back(&from_file, &day).await;

    std::fs::remove_dir_all(&dir).unwrap();
}
