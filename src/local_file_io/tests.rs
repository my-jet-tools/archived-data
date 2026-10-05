//! The contract of the IO traits, kept by local files: what `in_memory_io`'s tests prove of the test
//! implementation, proved of this one on a real directory — and what only a file system can get wrong.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::test_dir::TestDir;
use crate::{
    ArchiveFileReader, ArchiveFileWriter, IoReaderFactory, IoWriterFactory, LocalFileIo,
    LocalFileReader, LocalFileWriter,
};

/// What an archive does with its files, written generically over the traits, so the test also proves
/// every future involved is `Send`.
async fn write_then_read_back<TFactory, TWriter, TReader>(io: &TFactory) -> (u64, u64, Vec<u8>)
where
    TFactory: IoWriterFactory<TWriter> + IoReaderFactory<TReader>,
    TWriter: ArchiveFileWriter,
    TReader: ArchiveFileReader,
{
    let writer = io.create_data_writer(7).await.unwrap();
    let first = writer.append(b"hello").await.unwrap();
    let second = writer.append(b" world").await.unwrap();
    writer.write_at(0, b"J").await.unwrap();

    let reader = io.create_data_reader(7).await.unwrap();
    let read = reader.read_at(1, 4).await.unwrap();
    assert_eq!(reader.size().await.unwrap(), 11);
    assert_eq!(writer.size().await.unwrap(), 11);

    writer.finish().await.unwrap();
    (first, second, read)
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never happened"));
}

fn kind_of<T>(result: std::io::Result<T>) -> (std::io::ErrorKind, String) {
    match result {
        Ok(_) => panic!("not an error"),
        Err(error) => (error.kind(), error.to_string()),
    }
}

#[tokio::test]
async fn a_writer_appends_and_overwrites_and_a_reader_of_the_same_file_sees_it_all() {
    let dir = TestDir::new("contract");
    let io = Arc::new(LocalFileIo::new(dir.path()));
    let in_task = io.clone();

    let (first, second, read) = tokio::spawn(async move {
        write_then_read_back::<LocalFileIo, LocalFileWriter, LocalFileReader>(&in_task).await
    })
    .await
    .unwrap();

    assert_eq!((first, second), (0, 5));
    assert_eq!(read, b"ello");

    assert_eq!(std::fs::read(io.data_path(7)).unwrap(), b"Jello world");
    assert_eq!(io.counters().finishes(), 1);
    assert_eq!(io.counters().finishes_spawned_by_drop(), 0);
}

#[tokio::test]
async fn ranges_past_the_end_and_missing_files_are_errors() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = io.create_index_writer().await.unwrap();
    writer.append(b"abcd").await.unwrap();

    let (kind, message) = kind_of(writer.write_at(3, b"xy").await);
    assert_eq!(kind, std::io::ErrorKind::InvalidInput);
    assert!(message.contains("index"), "{message}");
    assert_eq!(
        kind_of(writer.write_at(u64::MAX, b"x").await).0,
        std::io::ErrorKind::InvalidInput
    );
    writer.write_at(4, b"").await.unwrap();
    assert_eq!(writer.size().await.unwrap(), 4);

    let reader = io.create_index_reader().await.unwrap();
    let (kind, message) = kind_of(reader.read_at(3, 2).await);
    assert_eq!(kind, std::io::ErrorKind::UnexpectedEof);
    assert!(message.contains("index"), "{message}");

    assert_eq!(
        kind_of(reader.read_at(u64::MAX, 1).await).0,
        std::io::ErrorKind::UnexpectedEof
    );
    assert_eq!(
        kind_of(reader.read_at(5, 0).await).0,
        std::io::ErrorKind::UnexpectedEof
    );
    assert_eq!(reader.read_at(4, 0).await.unwrap(), Vec::<u8>::new());
    assert_eq!(reader.read_at(0, 4).await.unwrap(), b"abcd");

    let (kind, message) = kind_of(io.create_data_reader(9).await);
    assert_eq!(kind, std::io::ErrorKind::NotFound);
    assert!(message.contains("data09"), "{message}");

    let nowhere = LocalFileIo::new(dir.join("no-such-directory"));
    assert_eq!(
        kind_of(nowhere.create_index_reader().await).0,
        std::io::ErrorKind::NotFound
    );
    assert!(!nowhere.dir().exists(), "a reader creates nothing");

    writer.finish().await.unwrap();
    assert_eq!(std::fs::read(io.index_path()).unwrap(), b"abcd");
}

#[tokio::test]
async fn creating_a_writer_starts_an_empty_file_and_a_reader_of_the_old_one_keeps_its_bytes() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let first = io.create_data_writer(0).await.unwrap();
    first.append(b"old").await.unwrap();
    first.finish().await.unwrap();
    let old = io.create_data_reader(0).await.unwrap();

    let second = io.create_data_writer(0).await.unwrap();
    assert_eq!(second.size().await.unwrap(), 0);
    assert_eq!(std::fs::read(io.data_path(0)).unwrap(), b"");
    assert_eq!(
        io.create_data_reader(0)
            .await
            .unwrap()
            .size()
            .await
            .unwrap(),
        0
    );

    second.append(b"new!").await.unwrap();
    second.finish().await.unwrap();
    assert_eq!(std::fs::read(io.data_path(0)).unwrap(), b"new!");

    // The old file is unlinked, not cut: what was open on it still reads it whole.
    #[cfg(unix)]
    {
        assert_eq!(old.size().await.unwrap(), 3);
        assert_eq!(old.read_at(0, 3).await.unwrap(), b"old");
    }

    drop(old);
}

#[tokio::test]
async fn a_second_finish_does_nothing_and_the_drop_after_it_spawns_nothing() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = io.create_data_writer(1).await.unwrap();
    writer.append(b"x").await.unwrap();
    writer.finish().await.unwrap();
    writer.finish().await.unwrap();
    drop(writer);

    assert_eq!(io.counters().finishes(), 1);
    assert_eq!(io.counters().finishes_spawned_by_drop(), 0);
    assert_eq!(std::fs::read(io.data_path(1)).unwrap(), b"x");
}

#[tokio::test]
async fn a_writer_dropped_without_finish_is_finished_by_drop() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = io.create_data_writer(2).await.unwrap();
    writer.append(b"abc").await.unwrap();
    drop(writer);

    assert_eq!(io.counters().finishes_spawned_by_drop(), 1);
    wait_until("the finish spawned by drop", || {
        io.counters().finishes() == 1
    })
    .await;
    assert_eq!(std::fs::read(io.data_path(2)).unwrap(), b"abc");
}

#[tokio::test]
async fn a_finish_awaited_after_the_drop_still_finishes_exactly_once() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = io.create_data_writer(3).await.unwrap();
    writer.append(b"zz").await.unwrap();

    let finishing = writer.finish();
    drop(writer);
    finishing.await.unwrap();

    wait_until("the finish", || io.counters().finishes() == 1).await;
    assert_eq!(io.counters().finishes_spawned_by_drop(), 1);
    assert_eq!(io.counters().finishes(), 1);
}

#[test]
fn a_writer_dropped_outside_any_runtime_is_still_finished() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let writer = io.create_data_writer(4).await.unwrap();
            writer.append(b"outlived").await.unwrap();
            writer
        });

    drop(writer);
    assert_eq!(io.counters().finishes_spawned_by_drop(), 1);

    let deadline = Instant::now() + Duration::from_secs(5);

    while io.counters().finishes() == 0 {
        assert!(Instant::now() < deadline, "the finish never happened");
        std::thread::yield_now();
    }

    assert_eq!(std::fs::read(io.data_path(4)).unwrap(), b"outlived");
}

#[tokio::test]
#[should_panic(expected = "already finished")]
async fn writing_to_a_finished_file_panics() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = io.create_data_writer(4).await.unwrap();
    writer.finish().await.unwrap();
    let _ = writer.append(b"late").await;
}

#[tokio::test]
async fn every_call_on_a_finished_file_panics_naming_it() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = Arc::new(io.create_data_writer(5).await.unwrap());
    writer.append(b"abc").await.unwrap();
    writer.finish().await.unwrap();

    let calls = [
        tokio::spawn({
            let writer = writer.clone();
            async move {
                let _ = writer.write_at(0, b"x").await;
            }
        }),
        tokio::spawn({
            let writer = writer.clone();
            async move {
                let _ = writer.size().await;
            }
        }),
    ];

    for call in calls {
        let panic = call.await.unwrap_err().into_panic();
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains("already finished"), "{message}");
        assert!(message.contains("data05"), "{message}");
    }

    assert_eq!(std::fs::read(io.data_path(5)).unwrap(), b"abc");
}

#[tokio::test]
async fn the_files_are_index_and_data_xx_in_a_directory_created_when_the_first_is() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.join("server/work/2026-09-15"));

    assert_eq!(io.index_path(), dir.join("server/work/2026-09-15/index"));
    assert_eq!(io.data_path(171), dir.join("server/work/2026-09-15/dataab"));
    assert!(!io.dir().exists());

    let mut writers = vec![io.create_index_writer().await.unwrap()];

    for no in [0, 10, 255] {
        writers.push(io.create_data_writer(no).await.unwrap());
    }

    for (n, writer) in writers.iter().enumerate() {
        writer.append(&[n as u8; 3]).await.unwrap();
        writer.finish().await.unwrap();
    }

    assert_eq!(
        TestDir::file_names(io.dir()),
        ["data00", "data0a", "dataff", "index"]
    );
    assert_eq!(std::fs::read(io.data_path(255)).unwrap(), [3, 3, 3]);
    assert_eq!(
        kind_of(io.create_data_reader(1).await).0,
        std::io::ErrorKind::NotFound
    );

    let clone = io.clone();
    let reader = clone.create_data_reader(10).await.unwrap();
    assert_eq!(reader.read_at(0, 3).await.unwrap(), [2, 2, 2]);
    assert_eq!(reader.path(), io.data_path(10));
}

#[tokio::test]
async fn a_file_that_cannot_be_created_is_an_error_naming_it() {
    let dir = TestDir::new("contract");
    std::fs::write(dir.join("a-file"), b"not a directory").unwrap();

    let io = LocalFileIo::new(dir.join("a-file"));
    let (_, message) = kind_of(io.create_index_writer().await);
    assert!(message.contains("a-file"), "{message}");

    let io = LocalFileIo::new(dir.path());
    std::fs::create_dir(io.data_path(0)).unwrap();
    let (_, message) = kind_of(io.create_data_writer(0).await);
    assert!(message.contains("data00"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_sees_what_is_written_after_it_opened_and_reads_run_side_by_side() {
    let dir = TestDir::new("contract");
    let io = LocalFileIo::new(dir.path());

    let writer = io.create_index_writer().await.unwrap();
    writer.append(&[0xAB]).await.unwrap();

    let reader = Arc::new(io.create_index_reader().await.unwrap());
    assert_eq!(reader.size().await.unwrap(), 1);

    let pattern: Vec<u8> = (0..(1usize << 20)).map(|at| (at % 251) as u8).collect();
    assert_eq!(writer.append(&pattern).await.unwrap(), 1);
    writer.write_at(0, &[0xCD]).await.unwrap();

    assert_eq!(reader.size().await.unwrap(), 1 + pattern.len() as u64);
    assert_eq!(reader.read_at(0, 1).await.unwrap(), [0xCD]);

    let reads: Vec<_> = (0..64u64)
        .map(|n| {
            let reader = reader.clone();
            let offset = 1 + n * 16_381;
            tokio::spawn(async move { (offset, reader.read_at(offset, 4_096).await.unwrap()) })
        })
        .collect();

    for read in reads {
        let (offset, bytes) = read.await.unwrap();
        let from = offset as usize - 1;
        assert!(bytes == pattern[from..from + 4_096], "the read at {offset}");
    }

    writer.finish().await.unwrap();
}
