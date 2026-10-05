use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, SplitDataWriters, SplitIndexWriter,
};

/// `finish` on the index, then on every data file in file order, each one awaited even after another
/// failed; the first failed finish is the error.
pub(crate) async fn finish_split_writers<TWriter: ArchiveFileWriter, TReader: ArchiveFileReader>(
    index: SplitIndexWriter<TWriter, TReader>,
    data_files: SplitDataWriters<TWriter>,
) -> Result<(), ArchiveError> {
    let mut finished = index.into_writer().finish().await;

    for writer in data_files.into_writers() {
        let result = writer.finish().await;

        if finished.is_ok() {
            finished = result;
        }
    }

    Ok(finished?)
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader, InMemoryWriter};
    use crate::{
        ArchiveError, DataFileLayout, SplitDataWriters, SplitIndexWriter, finish_split_writers,
    };

    async fn files(
        io: &InMemoryIo,
    ) -> (
        SplitIndexWriter<InMemoryWriter, InMemoryReader>,
        SplitDataWriters<InMemoryWriter>,
    ) {
        let index = SplitIndexWriter::create(io, &[1, 2, 3]).await.unwrap();
        let mut data = SplitDataWriters::new(DataFileLayout::new(10, 10));

        for rank in 0..3 {
            data.reserve_page(io, rank).await.unwrap();
        }

        (index, data)
    }

    fn failed_role(finished: Result<(), ArchiveError>) -> String {
        match finished {
            Err(ArchiveError::Io(error)) => error.to_string(),
            other => panic!("not an Io error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_file_is_finished_and_the_first_failure_is_returned() {
        let io = InMemoryIo::default();
        let (index, data) = files(&io).await;

        io.file(FileRole::Data(0)).unwrap().fail_finish();
        io.file(FileRole::Data(2)).unwrap().fail_finish();

        let message = failed_role(finish_split_writers(index, data).await);
        assert!(message.contains("Data(0)"), "{message}");

        for role in [
            FileRole::Index,
            FileRole::Data(0),
            FileRole::Data(1),
            FileRole::Data(2),
        ] {
            assert_eq!(io.file(role).unwrap().finish_attempts(), 1, "{role:?}");
        }
        assert_eq!(io.file(FileRole::Data(1)).unwrap().finishes(), 1);

        // The index is finished first, so its failure is the one returned, whichever data file fails too.
        let io = InMemoryIo::default();
        let (index, data) = files(&io).await;
        io.file(FileRole::Index).unwrap().fail_finish();
        io.file(FileRole::Data(1)).unwrap().fail_finish();

        let message = failed_role(finish_split_writers(index, data).await);
        assert!(message.contains("Index"), "{message}");
        for no in [0, 2] {
            assert_eq!(io.file(FileRole::Data(no)).unwrap().finishes(), 1);
        }
        assert_eq!(io.file(FileRole::Data(1)).unwrap().finish_attempts(), 1);

        let io = InMemoryIo::default();
        let (index, data) = files(&io).await;
        finish_split_writers(index, data).await.unwrap();
        assert_eq!(io.file(FileRole::Index).unwrap().finishes(), 1);
    }
}
