//! The use the archiver makes of local files: a day written to a working archive in one directory, compacted
//! into another, and read back — byte for byte what the same writes produce in memory.

use std::path::Path;
use std::sync::Arc;

use super::test_dir::TestDir;
use crate::fixed_key_minute_indexed_records::test_support::{
    Model, NAME, build, compact, data_files, day, file_bytes, moment, open, random_rows, record,
    seeded, time,
};
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::test_items::Login;
use crate::{
    ArchiveError, FixedKeyMinuteIndexedRecordsArchiveReader,
    FixedKeyMinuteIndexedRecordsArchiveWriter, LocalFileIo, LocalFileReader, LocalFileWriter,
    MinuteIndexedRecordsCompactionPlan, data_file_name, index_file_name,
};

type LocalWriter =
    FixedKeyMinuteIndexedRecordsArchiveWriter<Login, LocalFileIo, LocalFileWriter, LocalFileReader>;
type LocalReader = FixedKeyMinuteIndexedRecordsArchiveReader<Login, LocalFileIo, LocalFileReader>;

/// A finished working archive of `rows` in `io`'s directory, written exactly as `test_support::build` writes
/// one in memory: L0 frames of 4 intervals, L1 pages of `capacity_l1`, every cache holding `cache`.
async fn build_local(io: &LocalFileIo, rows: &[(u64, Vec<u8>)], capacity_l1: usize, cache: usize) {
    let mut writer = LocalWriter::new_with_capacity(io.clone(), NAME, day(), 4, capacity_l1)
        .await
        .unwrap();

    writer.set_l1_cache_size(cache);
    writer.set_l2_cache_size(cache);
    writer.set_data_cache_size(cache);

    for (login, record) in rows {
        writer.write(Login(*login), record).await.unwrap();
    }

    writer.finish().await.unwrap();
}

async fn open_local(io: &LocalFileIo) -> LocalReader {
    LocalReader::open(io.clone(), day()).await.unwrap()
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

/// The directory holds exactly the files of `plan`, under their names and of their planned sizes.
fn assert_files_of_plan(io: &LocalFileIo, plan: &MinuteIndexedRecordsCompactionPlan) {
    let mut names: Vec<String> = (0..plan.data_file_sizes().len())
        .map(|no| data_file_name(no as u8))
        .collect();
    names.push(index_file_name().to_string());
    names.sort();

    assert_eq!(TestDir::file_names(io.dir()), names);
    assert_eq!(
        std::fs::metadata(io.index_path()).unwrap().len(),
        plan.index_size()
    );

    for (no, size) in plan.data_file_sizes().iter().enumerate() {
        assert_eq!(
            std::fs::metadata(io.data_path(no as u8)).unwrap().len(),
            *size,
            "{}",
            data_file_name(no as u8)
        );
    }
}

/// Every file of `local` is the file of the same role in `memory`, byte for byte, and there are no others.
async fn assert_same_files(local: &LocalFileIo, memory: &InMemoryIo, case: &str) {
    assert!(
        read(&local.index_path()) == file_bytes(memory, FileRole::Index).await,
        "{case}: the index differs"
    );

    let data = data_files(memory).await;
    assert_eq!(
        TestDir::file_names(local.dir()).len(),
        data.len() + 1,
        "{case}: the files"
    );

    for (no, bytes) in data.iter().enumerate() {
        assert!(
            read(&local.data_path(no as u8)) == *bytes,
            "{case}: {} differs",
            data_file_name(no as u8)
        );
    }
}

/// Every login's day, and a spread of ranges, read concurrently off `reader` and compared with the model.
async fn assert_answers_every_key(reader: Arc<LocalReader>, model: &Model) {
    let mut reads = Vec::new();

    for login in model.logins.keys().copied().chain([1, 999_999]) {
        let reader = reader.clone();
        let expected_day = model.day(login);
        let expected_range = model.range(login, time(29, 0), time(700, 0));

        reads.push(tokio::spawn(async move {
            let day = reader.get_day(&Login(login)).await.unwrap();
            assert!(day == expected_day, "login {login}: its day");

            let range = reader
                .get_range(&Login(login), moment(29, 0), moment(700, 0))
                .await
                .unwrap();
            assert!(range == expected_range, "login {login}: minutes 29 to 700");
        }));
    }

    for read in reads {
        read.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_day_written_to_local_files_compacts_into_local_files_that_answer_every_key() {
    let dir = TestDir::new("round-trip");
    let working = LocalFileIo::new(dir.join("working"));
    let compacted = LocalFileIo::new(dir.join("compact"));

    let rows = random_rows(40, 4_000, &mut seeded(0x10CA1));
    let model = Model::of(&rows);

    // In a task of its own, so every future of an archive over local files is proved `Send`.
    let (plan, working, compacted) = tokio::spawn({
        let rows = rows.clone();

        async move {
            // Caches of 5: pages leave them and are read back through the local readers while the day is
            // written.
            build_local(&working, &rows, 4, 5).await;

            let reader = open_local(&working).await;
            let plan = reader.compaction_plan().await.unwrap();
            reader.write_compacted_to(&plan, &compacted).await.unwrap();

            (plan, working, compacted)
        }
    })
    .await
    .unwrap();

    assert_eq!(TestDir::file_names(working.dir()), ["data00", "index"]);
    assert_eq!(plan.records_count(), 4_000);
    assert_files_of_plan(&compacted, &plan);

    // The same writes over the in-memory files leave the same bytes, working and compact alike.
    let memory = build(&rows, 4, 5).await;
    assert_same_files(&working, &memory, "working").await;
    assert_same_files(&compacted, &compact(&open(&memory).await).await, "compact").await;

    let reader = Arc::new(open_local(&compacted).await);
    assert_eq!(reader.keys_count().await.unwrap(), model.logins.len());
    assert_eq!(reader.l0_frames(), 1);
    assert_answers_every_key(reader, &model).await;

    // The working archive answers the same.
    assert_answers_every_key(Arc::new(open_local(&working).await), &model).await;
}

#[tokio::test]
async fn several_data_files_are_named_in_order_and_a_compaction_run_again_replaces_them() {
    let dir = TestDir::new("round-trip");
    let working = LocalFileIo::new(dir.join("working"));
    let compacted = LocalFileIo::new(dir.join("compact"));

    let rows = random_rows(60, 3_000, &mut seeded(7));
    let model = Model::of(&rows);
    build_local(&working, &rows, 16, 8).await;

    let reader = open_local(&working).await;
    reader.set_max_data_file_bytes(150_000);
    let plan = reader.compaction_plan().await.unwrap();
    assert!(plan.data_file_sizes().len() > 3);

    reader.write_compacted_to(&plan, &compacted).await.unwrap();
    assert_files_of_plan(&compacted, &plan);

    let first: Vec<(String, Vec<u8>)> = TestDir::file_names(compacted.dir())
        .into_iter()
        .map(|name| {
            let bytes = read(&compacted.dir().join(&name));
            (name, bytes)
        })
        .collect();

    // A compaction is deterministic, and a run into the directory of an earlier one replaces every file.
    std::fs::write(compacted.data_path(0), b"left by a failed run").unwrap();
    std::fs::write(compacted.index_path(), b"left by a failed run").unwrap();
    reader.write_compacted_to(&plan, &compacted).await.unwrap();

    for (name, bytes) in &first {
        assert!(
            read(&compacted.dir().join(name)) == *bytes,
            "{name} is not what the first run wrote"
        );
    }

    assert_answers_every_key(Arc::new(open_local(&compacted).await), &model).await;
}

#[tokio::test]
async fn a_directory_with_no_index_or_with_an_unflushed_one_does_not_open() {
    let dir = TestDir::new("round-trip");

    let empty = LocalFileIo::new(dir.join("empty"));
    std::fs::create_dir_all(empty.dir()).unwrap();

    match LocalReader::open(empty.clone(), day()).await {
        Err(ArchiveError::Io(error)) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
        Err(other) => panic!("not a missing index: {other}"),
        Ok(_) => panic!("an empty directory opened"),
    }

    // Written, never flushed, dropped: the files are left behind, and the index refuses to open.
    let abandoned = LocalFileIo::new(dir.join("abandoned"));
    let mut writer = LocalWriter::new(abandoned.clone(), NAME, day())
        .await
        .unwrap();

    for id in 0..10 {
        writer
            .write(Login(5), &record(time(id, 0), id as u64, 64))
            .await
            .unwrap();
    }

    drop(writer);

    assert!(abandoned.index_path().exists());
    assert!(matches!(
        LocalReader::open(abandoned.clone(), day()).await,
        Err(ArchiveError::Corrupted(_))
    ));
}
