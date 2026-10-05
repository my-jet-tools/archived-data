use std::future::Future;

use crate::ArchiveFileWriter;

/// Creates the writers of ONE archive: its index, and its data files `0 ..= 255`.
///
/// The archive asks only for a role. What the file is called, where it lives and whether it is a file at
/// all is this factory's to resolve. `create_*` starts an EMPTY file; one that already exists under the
/// same name is replaced.
///
/// An archive that reads its own pages back while it writes them gets its readers from a factory that
/// implements [`crate::IoReaderFactory`] too, and such a reader sees every byte its writer has written.
///
/// An implementation writes both methods as a plain `async fn`. The futures are `Send` so an archive can
/// be written inside `tokio::spawn`.
pub trait IoWriterFactory<TWriter: ArchiveFileWriter> {
    fn create_index_writer(&self) -> impl Future<Output = std::io::Result<TWriter>> + Send;

    fn create_data_writer(&self, no: u8) -> impl Future<Output = std::io::Result<TWriter>> + Send;
}
