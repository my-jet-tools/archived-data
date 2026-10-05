use tokio::sync::OnceCell;

use crate::{ArchiveError, ArchiveFileReader, IoReaderFactory, MAX_DATA_FILES, SizedReader};

/// The data files of an archive being read, each opened on first use and kept for the reader's life:
/// opening is a factory call (for S3, a request), and one reader serves every concurrent read of its
/// file. A file that fails to open is not remembered, so the next use tries again.
pub(crate) struct LazyDataReaders<TReader> {
    files: Vec<OnceCell<SizedReader<TReader>>>,
}

impl<TReader: ArchiveFileReader> LazyDataReaders<TReader> {
    pub fn new() -> Self {
        Self {
            files: (0..MAX_DATA_FILES).map(|_| OnceCell::new()).collect(),
        }
    }

    pub async fn get<TFactory: IoReaderFactory<TReader>>(
        &self,
        factory: &TFactory,
        no: u8,
    ) -> Result<&SizedReader<TReader>, ArchiveError> {
        self.files[no as usize]
            .get_or_try_init(|| async {
                let reader = factory.create_data_reader(no).await?;
                SizedReader::open(reader).await
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader};
    use crate::{ArchiveError, LazyDataReaders};

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_uses_open_a_file_once() {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Data(3), vec![0; 77]);
        let readers = Arc::new(LazyDataReaders::<InMemoryReader>::new());

        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let io = io.clone();
                let readers = readers.clone();
                tokio::spawn(async move { readers.get(&io, 3).await.unwrap().size() })
            })
            .collect();

        for task in tasks {
            assert_eq!(task.await.unwrap(), 77);
        }

        assert_eq!(file.readers_opened(), 1);
    }

    #[tokio::test]
    async fn a_missing_file_is_an_error_that_is_not_remembered() {
        let io = InMemoryIo::default();
        let readers = LazyDataReaders::<InMemoryReader>::new();

        let missing = readers.get(&io, 255).await.map(|_| ());
        assert!(matches!(missing, Err(ArchiveError::Io(_))));

        let file = io.put(FileRole::Data(255), vec![1, 2, 3]);
        assert_eq!(readers.get(&io, 255).await.unwrap().size(), 3);
        assert_eq!(readers.get(&io, 255).await.unwrap().size(), 3);
        assert_eq!(file.readers_opened(), 1);
    }
}
