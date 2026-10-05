use std::future::Future;

/// One file of an archive being read, with random access.
///
/// The archive knows neither what the file is called nor whether it is a file at all: a local file reads
/// at an offset, an S3 object reads a range per call. Nothing here holds a position, so concurrent reads
/// of one file need not wait on each other — whether they do is the implementation's choice.
pub trait ArchiveFileReader: Send + Sync {
    /// Exactly `len` bytes from `offset`. A range reaching past the end of the file is an
    /// `UnexpectedEof` error, never a short read.
    fn read_at(
        &self,
        offset: u64,
        len: usize,
    ) -> impl Future<Output = std::io::Result<Vec<u8>>> + Send;

    /// How many bytes the file holds.
    fn size(&self) -> impl Future<Output = std::io::Result<u64>> + Send;
}
