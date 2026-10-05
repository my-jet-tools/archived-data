//! A working archive of records opens only when its last flush COMPLETED and nothing was written to its
//! files after it — the split working archive's rule, over an index that holds L2 frames and a data file of
//! frames that are rewritten in place.

use super::test_support::{
    L2, Model, NAME, NOT_USABLE, Reader, Writer, build, day, open, parse_archive, random_rows,
    record, refused, seeded, time,
};
use crate::ArchiveError;
use crate::in_memory_io::{FileRole, InMemoryIo};
use crate::test_items::Login;

async fn assert_refused(io: &InMemoryIo, case: &str) {
    match Reader::open(io.clone(), day()).await {
        Err(ArchiveError::Corrupted(message)) => {
            assert!(
                message.contains("L0 frame at 0 fails its header hash"),
                "{case}: {message}"
            )
        }
        Err(other) => panic!("{case}: {other}"),
        Ok(_) => panic!("{case}: opened"),
    }
}

async fn assert_opens_with(io: &InMemoryIo, model: &Model, case: &str) {
    let reader = Reader::open(io.clone(), day())
        .await
        .unwrap_or_else(|error| panic!("{case}: {error}"));

    assert_eq!(
        reader.keys_count().await.unwrap(),
        model.logins.len(),
        "{case}"
    );

    for (login, records) in &model.logins {
        assert!(
            reader.get_day(&Login(*login)).await.unwrap() == *records,
            "{case}: login {login}"
        );
    }
}

#[tokio::test]
async fn an_archive_opens_at_every_completed_flush_and_is_refused_between_them() {
    let mut rng = seeded(7);
    let rows = random_rows(6, 600, &mut rng);

    let io = InMemoryIo::default();
    let mut writer = Writer::new_with_capacity(io.clone(), NAME, day(), 2, 2)
        .await
        .unwrap();

    // Nothing is kept between writes, so every write reaches the files at once.
    writer.set_l1_cache_size(1);
    writer.set_l2_cache_size(0);
    writer.set_data_cache_size(0);

    assert_refused(&io, "never flushed").await;

    let mut model = Model::default();

    for (n, chunk) in rows.chunks(150).enumerate() {
        for (login, record) in chunk {
            writer.write(Login(*login), record).await.unwrap();
            model.push(*login, record);
        }

        assert_refused(&io, &format!("chunk {n} written, not flushed")).await;

        writer.flush().await.unwrap();
        assert_opens_with(&io, &model, &format!("chunk {n} flushed")).await;
        parse_archive(&io).await;

        // A flush with nothing written since writes nothing.
        let index = io.file(FileRole::Index).unwrap();
        let before = (index.appends(), index.write_ats());
        writer.flush().await.unwrap();
        assert_eq!((index.appends(), index.write_ats()), before);
    }

    writer.finish().await.unwrap();
    assert_opens_with(&io, &model, "finished").await;
}

#[tokio::test]
async fn a_write_that_stays_in_the_caches_leaves_the_last_flush_readable() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone(), NAME, day()).await.unwrap();

    writer
        .write(Login(1), &record(time(1, 0), 1, 100))
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let flushed = Model::of(&[(1, record(time(1, 0), 1, 100))]);
    assert_opens_with(&io, &flushed, "flushed").await;

    // Default caches: the record, its frame and its L2 frame stay in memory.
    writer
        .write(Login(1), &record(time(0, 0), 2, 100))
        .await
        .unwrap();
    writer
        .write(Login(2), &record(time(5, 0), 3, 100))
        .await
        .unwrap();
    assert_opens_with(&io, &flushed, "written into the caches only").await;

    writer.finish().await.unwrap();

    let reader = open(&io).await;
    assert_eq!(reader.keys_count().await.unwrap(), 2);
    assert_eq!(reader.get_day(&Login(1)).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_writer_dropped_after_writing_leaves_an_index_refused() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone(), NAME, day()).await.unwrap();
    writer.set_data_cache_size(0);

    writer
        .write(Login(1), &record(time(1, 0), 1, 100))
        .await
        .unwrap();
    writer.flush().await.unwrap();

    writer
        .write(Login(1), &record(time(2, 0), 2, 100))
        .await
        .unwrap();
    drop(writer);

    assert_refused(&io, "a frame rewritten after the flush").await;
}

#[tokio::test]
async fn a_failed_write_leaves_the_writer_unusable_and_the_index_refused() {
    let io = InMemoryIo::default();
    let mut writer = Writer::new(io.clone(), NAME, day()).await.unwrap();
    writer.set_data_cache_size(0);

    writer
        .write(Login(1), &record(time(1, 0), 1, 100))
        .await
        .unwrap();
    writer.flush().await.unwrap();

    io.file(FileRole::Data(0)).unwrap().fail_write_ats();

    assert!(matches!(
        writer.write(Login(1), &record(time(2, 0), 2, 100)).await,
        Err(ArchiveError::Io(_))
    ));

    assert_eq!(
        refused(writer.write(Login(1), &record(time(3, 0), 3, 100)).await),
        NOT_USABLE
    );
    assert_eq!(refused(writer.flush().await), NOT_USABLE);

    // Refused before the record is looked at: no panic for a time outside the day, no refusal of its length.
    assert_eq!(
        refused(writer.write(Login(1), &record(time(-1, 0), 4, 100)).await),
        NOT_USABLE
    );
    assert_eq!(refused(writer.write(Login(1), &[0; 3]).await), NOT_USABLE);

    assert_eq!(refused(writer.finish().await), NOT_USABLE);

    assert_refused(&io, "the spoil landed, the frame did not").await;
}

/// A working index proves it is whole by frame 0's hash, and nothing at open looks past its last L1 page: an
/// index cut inside an L2 frame that lies after that page opens, and the key of that frame fails at its first
/// read. Found in review; a COMPACT index puts its L1 pages last, where the check at open does look.
#[tokio::test]
async fn a_working_index_cut_inside_a_trailing_l2_frame_opens_and_fails_at_that_key() {
    // L0 frame 0 · L2 of key 1 · the L1 page · L2 of key 2 · L2 of key 3.
    let rows = vec![
        (1, record(time(1, 0), 1, 40)),
        (2, record(time(2, 0), 2, 40)),
        (3, record(time(3, 0), 3, 40)),
    ];
    let io = build(&rows, 1024, 64).await;

    let index = io.file(FileRole::Index).unwrap().bytes().await;
    let data = io.file(FileRole::Data(0)).unwrap().bytes().await;
    let parsed = parse_archive(&io).await;
    assert!(
        parsed[&3].l2_offset as usize + L2 == index.len(),
        "key 3's L2 frame ends the index"
    );

    let cut = InMemoryIo::default();
    cut.put(FileRole::Index, index[..index.len() - 100].to_vec());
    cut.put(FileRole::Data(0), data);

    let reader = Reader::open(cut, day()).await.unwrap();
    assert_eq!(reader.get_day(&Login(1)).await.unwrap().len(), 1);
    assert_eq!(reader.get_day(&Login(2)).await.unwrap().len(), 1);
    assert!(matches!(
        reader.get_day(&Login(3)).await,
        Err(ArchiveError::Corrupted(message)) if message.contains("past the end")
    ));
}
