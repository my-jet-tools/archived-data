use std::future::Future;

use crate::ArchiveFileReader;

/// Creates the readers of ONE archive: its index, and its data files `0 ..= 255`.
///
/// The archive asks only for a role. What the file is called, where it lives and whether it is a file at
/// all is this factory's to resolve. A file that does not exist is an `Err`, never an empty reader.
///
/// An implementation writes both methods as a plain `async fn`. The futures are `Send` so an archive can
/// be read inside `tokio::spawn`.
pub trait IoReaderFactory<TReader: ArchiveFileReader> {
    fn create_index_reader(&self) -> impl Future<Output = std::io::Result<TReader>> + Send;

    fn create_data_reader(&self, no: u8) -> impl Future<Output = std::io::Result<TReader>> + Send;
}
