use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};

use super::test_types::{Hour, Name, Names, Record, Rng, reference_compact};
use crate::{
    ArchiveError, VariableKeyPreallocatedSlotsArchiveReaderSingleFile,
    VariableKeyPreallocatedSlotsArchiveWriterSingleFile, decode_key_prefix, encode_key_prefix,
    key_prefix_len,
};

type Writer<T> = VariableKeyPreallocatedSlotsArchiveWriterSingleFile<Name, Hour, Record, T>;
type Reader<T> = VariableKeyPreallocatedSlotsArchiveReaderSingleFile<Name, Hour, Record, T>;

type Row = (Vec<u8>, usize, Record);

fn seeded(seed: u64) -> Rng {
    Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
}

/// `count` distinct keys of 1..=`max_len` random bytes, zero bytes included.
fn random_keys(count: usize, max_len: u64, rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut keys = BTreeSet::new();

    while keys.len() < count {
        let len = 1 + rng.next() % max_len;
        keys.insert((0..len).map(|_| rng.next() as u8).collect::<Vec<u8>>());
    }

    keys.into_iter().collect()
}

/// `key-00000`, `key-00001`, …: nine bytes each, ascending in their byte order.
fn numbered_keys(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| format!("key-{i:05}").into_bytes())
        .collect()
}

/// Every key × up to three distinct hours, fully shuffled. Each slot is written once, so every order of
/// the rows describes the same file.
fn rows(keys: &[Vec<u8>], rng: &mut Rng) -> (Vec<Row>, Names) {
    let mut rows = Vec::new();
    let mut names: Names = BTreeMap::new();

    for key in keys {
        let hours = names.entry(key.clone()).or_insert_with(|| vec![None; 24]);

        for _ in 0..3 {
            let hour = (rng.next() % 24) as usize;

            if hours[hour].is_some() {
                continue;
            }

            let record = Record::with(rng.next() % 1_000_000);
            hours[hour] = Some(record.clone());
            rows.push((key.clone(), hour, record));
        }
    }

    rng.shuffle(&mut rows);
    (rows, names)
}

async fn write_all<T: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
    writer: &mut Writer<T>,
    rows: &[Row],
) {
    for (key, hour, record) in rows {
        writer
            .write(Name(key.clone()), Hour(*hour), record)
            .await
            .unwrap();
    }
}

async fn build(rows: &[Row], l0_frame_bytes: usize, l1_page_bytes: usize, cache: usize) -> Vec<u8> {
    let mut writer =
        Writer::new_with_capacity(Cursor::new(Vec::new()), l0_frame_bytes, l1_page_bytes);
    writer.set_l1_cache_size(cache);
    writer.set_data_cache_size(cache);
    write_all(&mut writer, rows).await;
    writer.finish().await.unwrap().into_inner()
}

async fn assert_reads_back<T: AsyncRead + AsyncSeek + Unpin>(reader: &Reader<T>, names: &Names) {
    for (key, hours) in names {
        let items = reader
            .get(&Name(key.clone()), Hour(0), Hour(23))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("key {key:?} is absent"));

        let expected: Vec<Record> = hours
            .iter()
            .map(|hour| hour.clone().unwrap_or_default())
            .collect();
        assert!(items == expected, "key {key:?}");
    }
}

async fn compact_of<T: AsyncRead + AsyncSeek + Unpin>(reader: &Reader<T>) -> Vec<u8> {
    let mut out = Vec::new();
    let written = reader.write_compacted_to(&mut out).await.unwrap();
    assert_eq!(written, out.len() as u64);
    assert_eq!(reader.compacted_len().await.unwrap(), written);
    out
}

fn le_u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
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

/// `(size, count)` of every L1 page the reader's L0 points at.
fn l1_headers<T>(reader: &Reader<T>, bytes: &[u8]) -> Vec<(usize, usize)> {
    reader
        .l0
        .iter()
        .map(|interval| {
            let at = interval.offset as usize;
            (le_u32_at(bytes, at), le_u32_at(bytes, at + 4))
        })
        .collect()
}

#[test]
fn the_length_prefix_round_trips_at_every_boundary() {
    let mut end = Vec::new();
    encode_key_prefix(0, &mut end);
    assert_eq!(end, vec![0x00], "length 0 is the end marker");
    assert!(decode_key_prefix(&end, 0).unwrap().is_none());

    let cases: [(usize, &[u8]); 6] = [
        (1, &[0x01]),
        (127, &[0x7F]),
        (128, &[0x80, 0x80]),
        (255, &[0x80, 0xFF]),
        (256, &[0x81, 0x00]),
        (32767, &[0xFF, 0xFF]),
    ];

    for (len, spelled) in cases {
        let mut out = vec![0xAA];
        encode_key_prefix(len, &mut out);
        assert_eq!(&out[1..], spelled, "length {len}");
        assert_eq!(key_prefix_len(len), spelled.len());
        assert_eq!(
            decode_key_prefix(&out, 1).unwrap(),
            Some((len, spelled.len())),
            "length {len}"
        );
    }

    assert!(
        matches!(
            decode_key_prefix(&[0x80, 0x05], 0),
            Err(ArchiveError::Corrupted(_))
        ),
        "a short length spelled in two bytes"
    );
    assert!(
        matches!(
            decode_key_prefix(&[0x81], 0),
            Err(ArchiveError::Corrupted(_))
        ),
        "a two-byte length cut short"
    );
}

#[tokio::test]
async fn empty_and_over_half_page_keys_are_refused_before_anything_is_written() {
    let mut writer = Writer::new(Cursor::new(Vec::new()));

    // A default L1 page is 16 KiB: an entry may be (16384 − 8) / 2 = 8188 bytes, so 2 + 8178 + 8.
    for key in [vec![], vec![7u8; 8179], vec![7u8; 20_000]] {
        let len = key.len();
        assert!(
            matches!(
                writer.write(Name(key), Hour(0), &Record::with(1)).await,
                Err(ArchiveError::ContractViolation(_))
            ),
            "a key of {len} bytes was accepted"
        );
    }
    assert_eq!(writer.keys_count(), 0);
    assert_eq!(writer.l0_intervals(), 0);

    writer
        .write(Name(vec![7u8; 8178]), Hour(2), &Record::with(2))
        .await
        .unwrap();
    writer
        .write(Name(vec![1]), Hour(3), &Record::with(3))
        .await
        .unwrap();

    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();
    assert_eq!(reader.keys_count().await.unwrap(), 2);
    assert_eq!(
        reader
            .get(&Name(vec![7u8; 8178]), Hour(2), Hour(2))
            .await
            .unwrap(),
        Some(vec![Record::with(2)])
    );
}

#[tokio::test]
async fn the_largest_key_is_32767_bytes_and_compacts_into_a_page_of_its_own() {
    // Pages large enough that only the length limit, not the half-page rule, decides.
    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 1 << 17, 1 << 17);

    assert!(matches!(
        writer
            .write(Name(vec![9u8; 32768]), Hour(0), &Record::with(1))
            .await,
        Err(ArchiveError::ContractViolation(_))
    ));

    let mut names: Names = BTreeMap::new();
    for (key, hour) in [
        (vec![9u8; 32767], 4),
        (vec![8u8; 128], 5),
        (vec![8u8; 127], 6),
        (vec![3], 7),
    ] {
        writer
            .write(Name(key.clone()), Hour(hour), &Record::with(hour as u64))
            .await
            .unwrap();
        names.entry(key).or_insert_with(|| vec![None; 24])[hour] = Some(Record::with(hour as u64));
    }

    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();
    assert_reads_back(&reader, &names).await;

    let compact = compact_of(&reader).await;
    assert!(compact == reference_compact(&names));

    let reader = Reader::open(Cursor::new(compact.clone())).await.unwrap();
    let headers = l1_headers(&reader, &compact);
    assert_eq!(
        headers.last().unwrap(),
        &(8 + 2 + 32767 + 8, 1),
        "an entry past 16 KiB still gets a page"
    );
    assert_reads_back(&reader, &names).await;
}

#[tokio::test]
async fn random_insert_orders_and_key_lengths_with_splits_read_back() {
    for seed in [1u64, 7, 42, 2026] {
        let mut rng = seeded(seed);
        let keys = random_keys(300, 300, &mut rng);
        let (rows, names) = rows(&keys, &mut rng);

        // 1 KiB pages split constantly and hold one or two intervals per L0 frame; cache 2 forces
        // evictions and reloads of both page kinds.
        let bytes = build(&rows, 1024, 1024, 2).await;
        let reader = Reader::open(Cursor::new(bytes.clone())).await.unwrap();

        assert!(reader.l0_intervals() > 10, "the test must split");
        assert!(reader.l0_frames() > 3, "the test must chain");
        assert_eq!(frame_offsets(&bytes).len(), reader.l0_frames());
        assert_eq!(reader.keys_count().await.unwrap(), names.len());
        assert!(
            l1_headers(&reader, &bytes)
                .iter()
                .all(|(size, _)| *size == 1024)
        );
        assert_reads_back(&reader, &names).await;

        for absent in random_keys(50, 300, &mut rng) {
            if !names.contains_key(&absent) {
                assert_eq!(
                    reader.get(&Name(absent), Hour(0), Hour(23)).await.unwrap(),
                    None
                );
            }
        }
    }
}

#[tokio::test]
async fn a_split_by_bytes_leaves_two_pages_that_fit() {
    let mut rng = seeded(64);

    // A 64-byte page takes 56 bytes of entries, and an entry may be 28: a 19-byte key.
    let mut keys: Vec<Vec<u8>> = (0..60u8).map(|i| vec![i; 19]).collect();
    keys.extend((60..120u8).map(|i| vec![i; 3]));
    let (rows, names) = rows(&keys, &mut rng);

    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 64, 64);
    writer.set_l1_cache_size(1);
    writer.set_data_cache_size(1);

    assert!(matches!(
        writer
            .write(Name(vec![0; 20]), Hour(0), &Record::default())
            .await,
        Err(ArchiveError::ContractViolation(_))
    ));

    write_all(&mut writer, &rows).await;
    let bytes = writer.finish().await.unwrap().into_inner();
    let reader = Reader::open(Cursor::new(bytes.clone())).await.unwrap();

    let headers = l1_headers(&reader, &bytes);
    assert!(headers.len() > 20, "the test must split");
    for (size, count) in headers {
        // A page holds 56 bytes of entries: at most two 28-byte entries, at most four 12-byte ones.
        assert_eq!(size, 64);
        assert!((1..=4).contains(&count));
    }
    assert_eq!(reader.keys_count().await.unwrap(), names.len());
    assert_reads_back(&reader, &names).await;
}

#[tokio::test]
async fn the_writer_reports_its_keys_and_overwrites_a_known_key_in_place() {
    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 64, 64);

    for (key, value) in [
        (&b"alpha-key"[..], 1),
        (b"bravo-key", 2),
        (b"delta-key", 3),
        (b"bravo-key", 4),
        (b"alpha-key", 5),
        (b"charl-key", 6),
    ] {
        writer
            .write(Name(key.to_vec()), Hour(1), &Record::with(value))
            .await
            .unwrap();
    }

    // Three 18-byte entries fill a 64-byte page; the fourth key splits it.
    assert_eq!(writer.keys_count(), 4);
    assert_eq!(writer.l0_intervals(), 2);

    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();
    assert_eq!(
        reader
            .get(&Name(b"alpha-key".to_vec()), Hour(1), Hour(1))
            .await
            .unwrap(),
        Some(vec![Record::with(5)])
    );
}

#[tokio::test]
async fn l0_chains_across_several_frames_and_reads_back() {
    for seed in [3u64, 11, 500] {
        let mut rng = seeded(seed);
        let (rows, names) = rows(&numbered_keys(400), &mut rng);

        // A 28-byte interval, two to an 80-byte frame; three keys to a 64-byte L1 page.
        let bytes = build(&rows, 80, 64, 2).await;
        let reader = Reader::open(Cursor::new(bytes.clone())).await.unwrap();

        assert!(reader.l0_frames() > 3, "the test must chain");
        assert_eq!(reader.l0_frames(), reader.l0_intervals().div_ceil(2));
        assert_eq!(frame_offsets(&bytes).len(), reader.l0_frames());
        assert_eq!(reader.keys_count().await.unwrap(), names.len());
        assert_reads_back(&reader, &names).await;

        let compact = compact_of(&reader).await;
        assert!(
            compact == reference_compact(&names),
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
    let mut rng = seeded(77);
    let (rows, names) = rows(&numbered_keys(300), &mut rng);

    let mut writer = Writer::new_with_capacity(Cursor::new(Vec::new()), 80, 64);
    writer.set_data_cache_size(4);

    let mut frames = 0;
    for chunk in rows.chunks(rows.len() / 5 + 1) {
        write_all(&mut writer, chunk).await;
        writer.flush().await.unwrap();

        assert!(writer.l0_frames() >= frames, "a frame is never given back");
        assert_eq!(writer.l0_frames(), writer.l0_intervals().div_ceil(2));
        frames = writer.l0_frames();
    }

    let bytes = writer.finish().await.unwrap().into_inner();
    let offsets = frame_offsets(&bytes);
    assert_eq!(offsets.len(), frames);
    assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));

    let reader = Reader::open(Cursor::new(bytes)).await.unwrap();
    assert_reads_back(&reader, &names).await;
}

#[tokio::test]
async fn a_corrupted_l0_chain_is_refused() {
    let mut rng = seeded(5150);
    let (rows, _) = rows(&numbered_keys(120), &mut rng);
    let bytes = build(&rows, 80, 64, 2).await;

    let offsets = frame_offsets(&bytes);
    assert!(offsets.len() >= 3, "the test needs three frames");

    let with_next = |frame: usize, next: u64| {
        let mut bytes = bytes.clone();
        let at = offsets[frame] + 4;
        bytes[at..at + 8].copy_from_slice(&next.to_le_bytes());
        bytes
    };

    let with_count = |count: usize| {
        let mut bytes = bytes.clone();
        bytes[12..16].copy_from_slice(&(count as u32).to_le_bytes());
        bytes
    };
    let count = le_u32_at(&bytes, 12);

    // The first `from` of frame 1 below everything in frame 0: `a` sorts before `k`.
    let mut unordered = bytes.clone();
    unordered[offsets[1] + 16 + 1] = b'a';

    let cases = [
        ("a next into frame 0", with_next(0, 8)),
        ("a next to itself", with_next(1, offsets[1] as u64)),
        (
            "a next back to an earlier frame",
            with_next(2, offsets[1] as u64),
        ),
        (
            "a next into frame 0 from a continuation",
            with_next(2, 0x10),
        ),
        (
            "a next past the end",
            with_next(1, bytes.len() as u64 + 4096),
        ),
        ("a count above the entries", with_count(count + 1)),
        ("a count below the entries", with_count(count - 1)),
        ("intervals unordered across frames", unordered),
    ];

    for (case, bytes) in cases {
        assert!(
            matches!(
                Reader::open(Cursor::new(bytes)).await,
                Err(ArchiveError::Corrupted(_))
            ),
            "{case} was accepted"
        );
    }
}

#[tokio::test]
async fn an_l1_page_whose_count_disagrees_with_its_entries_is_corrupted() {
    let mut rng = seeded(8);
    let (rows, names) = rows(&numbered_keys(10), &mut rng);
    let bytes = build(&rows, 1024, 64, 2).await;

    let reader = Reader::open(Cursor::new(bytes.clone())).await.unwrap();
    let at = reader.l0[0].offset as usize + 4;
    let first = Name(names.keys().next().unwrap().clone());

    for count in [le_u32_at(&bytes, at) + 1, le_u32_at(&bytes, at) - 1] {
        let mut bytes = bytes.clone();
        bytes[at..at + 4].copy_from_slice(&(count as u32).to_le_bytes());
        let reader = Reader::open(Cursor::new(bytes)).await.unwrap();

        assert!(matches!(
            reader.get(&first, Hour(0), Hour(0)).await,
            Err(ArchiveError::Corrupted(_))
        ));
        assert!(matches!(
            reader.write_compacted_to(&mut Vec::new()).await,
            Err(ArchiveError::Corrupted(_))
        ));
    }
}

#[tokio::test]
async fn a_slot_out_of_range_is_refused_on_write_and_read() {
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    let result = writer
        .write(Name(b"one".to_vec()), Hour(24), &Record::default())
        .await;
    assert!(matches!(result, Err(ArchiveError::ContractViolation(_))));

    writer
        .write(Name(b"one".to_vec()), Hour(0), &Record::with(1))
        .await
        .unwrap();
    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();

    assert!(matches!(
        reader.get(&Name(b"one".to_vec()), Hour(0), Hour(24)).await,
        Err(ArchiveError::ContractViolation(_))
    ));
}

#[tokio::test]
async fn get_is_inclusive_empty_when_from_is_after_to_and_none_for_an_absent_key() {
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    for hour in [3, 4, 6] {
        writer
            .write(
                Name(b"mid".to_vec()),
                Hour(hour),
                &Record::with(hour as u64),
            )
            .await
            .unwrap();
    }
    writer
        .write(Name(b"top".to_vec()), Hour(0), &Record::with(9))
        .await
        .unwrap();
    let reader = Reader::open(Cursor::new(writer.finish().await.unwrap().into_inner()))
        .await
        .unwrap();

    assert_eq!(
        reader
            .get(&Name(b"mid".to_vec()), Hour(5), Hour(4))
            .await
            .unwrap(),
        Some(vec![])
    );
    assert_eq!(
        reader
            .get(&Name(b"zzz".to_vec()), Hour(5), Hour(4))
            .await
            .unwrap(),
        Some(vec![])
    );
    assert!(
        reader.l1_cache.pages.read().await.is_empty(),
        "nothing was read"
    );

    assert_eq!(
        reader
            .get(&Name(b"mid".to_vec()), Hour(3), Hour(6))
            .await
            .unwrap(),
        Some(vec![
            Record::with(3),
            Record::with(4),
            Record::default(),
            Record::with(6)
        ])
    );
    assert_eq!(
        reader
            .get(&Name(b"mid".to_vec()), Hour(4), Hour(4))
            .await
            .unwrap(),
        Some(vec![Record::with(4)])
    );

    for absent in [&b"a"[..], b"mi", b"mid\0", b"tops", b"zzz"] {
        assert_eq!(
            reader
                .get(&Name(absent.to_vec()), Hour(0), Hour(23))
                .await
                .unwrap(),
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
    assert_eq!(
        reader
            .get(&Name(b"x".to_vec()), Hour(0), Hour(0))
            .await
            .unwrap(),
        None
    );
    assert_eq!(reader.compacted_len().await.unwrap(), 16);
    assert_eq!(
        compact_of(&reader).await,
        reference_compact(&BTreeMap::new())
    );
}

#[tokio::test]
async fn a_truncated_or_garbage_file_is_an_error_not_an_absent_key() {
    assert!(matches!(
        Reader::open(Cursor::new(vec![1, 2, 3])).await,
        Err(ArchiveError::Corrupted(_))
    ));

    let mut writer = Writer::new(Cursor::new(Vec::new()));
    writer
        .write(Name(b"five".to_vec()), Hour(0), &Record::with(5))
        .await
        .unwrap();
    let bytes = writer.finish().await.unwrap().into_inner();

    let cut = bytes[..bytes.len() - 100].to_vec();
    let reader = Reader::open(Cursor::new(cut)).await.unwrap();
    assert!(
        reader
            .get(&Name(b"five".to_vec()), Hour(0), Hour(0))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn compact_output_is_identical_whatever_the_insert_order_and_page_sizes() {
    let mut rng = seeded(0xDEAD_BEEF);
    let keys = random_keys(2_500, 40, &mut rng);
    let (rows, names) = rows(&keys, &mut rng);
    let reference = reference_compact(&names);

    let mut reversed = rows.clone();
    reversed.reverse();

    for (rows, l0, l1, cache) in [
        (&rows, 64 * 1024, 16 * 1024, 64),
        (&reversed, 4096, 1024, 3),
        (&rows, 1024, 256, 1),
    ] {
        let bytes = build(rows, l0, l1, cache).await;
        let reader = Reader::open(Cursor::new(bytes)).await.unwrap();

        let out = compact_of(&reader).await;
        assert!(out == reference, "compact bytes differ (l1 page {l1})");

        // A compact file is itself a readable file, and compacts to itself.
        let compact = Reader::open(Cursor::new(out.clone())).await.unwrap();
        assert_eq!(compact.l0_frames(), 1);
        assert!(compact.l0_intervals() > 1);
        assert_eq!(compact.keys_count().await.unwrap(), names.len());
        assert_reads_back(&compact, &names).await;
        assert!(compact_of(&compact).await == out);
    }
}

#[tokio::test]
async fn a_compact_l1_page_is_capped_by_1024_keys_and_by_16_kib() {
    let mut rng = seeded(1024);

    // Four-byte keys: 13-byte entries, so 1024 of them are 13 320 bytes and the key cap closes pages.
    let short: Vec<Vec<u8>> = (0..2_500).map(|i| format!("{i:04}").into_bytes()).collect();
    // 200-byte keys: 210-byte entries, so 77 of them are 16 178 bytes and a 78th would pass 16 384.
    let long: Vec<Vec<u8>> = (0..300)
        .map(|i| {
            let mut key = format!("{i:05}").into_bytes();
            key.resize(200, b'x');
            key
        })
        .collect();

    for (keys, entry, expected) in [
        (short, 13, vec![1024, 1024, 452]),
        (long, 210, vec![77, 77, 77, 69]),
    ] {
        let (rows, names) = rows(&keys, &mut rng);
        let reader = Reader::open(Cursor::new(build(&rows, 64 * 1024, 16 * 1024, 16).await))
            .await
            .unwrap();
        let compact = compact_of(&reader).await;
        assert!(compact == reference_compact(&names));

        let reader = Reader::open(Cursor::new(compact.clone())).await.unwrap();
        let headers = l1_headers(&reader, &compact);

        assert_eq!(
            headers.iter().map(|(_, count)| *count).collect::<Vec<_>>(),
            expected
        );
        for (size, count) in headers {
            assert_eq!(size, 8 + count * entry, "no padding, no end marker");
            assert!(size <= 16 * 1024);
        }
        assert_reads_back(&reader, &names).await;
    }
}

#[tokio::test]
async fn the_reader_caches_are_bounded_by_page_count() {
    let mut rng = seeded(10);
    let (rows, names) = rows(&numbered_keys(40), &mut rng);
    let reader = Reader::open(Cursor::new(build(&rows, 1024, 64, 2).await))
        .await
        .unwrap();

    reader.set_l1_cache_size(1);
    reader.set_data_cache_size(2);
    assert_reads_back(&reader, &names).await;

    assert_eq!(reader.l1_cache.pages.read().await.len(), 1);
    assert_eq!(reader.data_cache.pages.read().await.len(), 2);

    reader.set_l1_cache_size(0);
    reader.set_data_cache_size(0);
    assert_reads_back(&reader, &names).await;
}

#[tokio::test]
async fn through_tokio_files_and_an_arc_shared_compaction_task() {
    let dir = std::env::temp_dir().join(format!(
        "archived-data-variable-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let working = dir.join("file.temp");

    let mut rng = seeded(314);
    let keys = random_keys(1_500, 200, &mut rng);
    let (rows, names) = rows(&keys, &mut rng);

    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&working)
        .await
        .unwrap();

    let mut writer = Writer::new_with_capacity(file, 4096, 2048);
    writer.set_l1_cache_size(4);
    writer.set_data_cache_size(100);
    write_all(&mut writer, &rows).await;
    writer.finish().await.unwrap().sync_all().await.unwrap();

    let reader = Arc::new(
        Reader::open(tokio::fs::File::open(&working).await.unwrap())
            .await
            .unwrap(),
    );
    assert_reads_back(&reader, &names).await;

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

    assert!(compact == reference_compact(&names));

    let compact_path = dir.join("file");
    std::fs::write(&compact_path, &compact).unwrap();
    let from_file = Reader::open(tokio::fs::File::open(&compact_path).await.unwrap())
        .await
        .unwrap();
    assert_reads_back(&from_file, &names).await;

    std::fs::remove_dir_all(&dir).unwrap();
}
