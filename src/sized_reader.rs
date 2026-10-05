use crate::{ArchiveError, ArchiveFileReader};

/// A reader of a file that does not change while it is read, with its size learned once. `read_at` is
/// exact, so a speculative read is clamped to the bytes that remain instead of reaching past the end.
pub(crate) struct SizedReader<TReader> {
    reader: TReader,
    size: u64,
}

impl<TReader: ArchiveFileReader> SizedReader<TReader> {
    pub async fn open(reader: TReader) -> Result<Self, ArchiveError> {
        let size = reader.size().await?;
        Ok(Self { reader, size })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Up to `len` bytes from `offset`, fewer only where the file ends: at most one `read_at`, none at or
    /// past the end.
    pub async fn read_up_to(&self, offset: u64, len: usize) -> Result<Vec<u8>, ArchiveError> {
        let len = (len as u64).min(self.size.saturating_sub(offset)) as usize;

        if len == 0 {
            return Ok(Vec::new());
        }

        Ok(self.reader.read_at(offset, len).await?)
    }

    /// Exactly `len` bytes. Every offset asked for comes out of the archive's own bytes, so a range the
    /// file does not hold is `Corrupted`, and is refused without a read.
    pub async fn read_exact(&self, offset: u64, len: usize) -> Result<Vec<u8>, ArchiveError> {
        let fits = offset
            .checked_add(len as u64)
            .is_some_and(|end| end <= self.size);

        if !fits {
            return Err(ArchiveError::Corrupted(format!(
                "{len} bytes at {offset} reach past the end of a {}-byte file",
                self.size
            )));
        }

        Ok(self.reader.read_at(offset, len).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IoReaderFactory;
    use crate::in_memory_io::{FileRole, InMemoryIo};

    #[tokio::test]
    async fn a_read_is_clamped_to_the_file_and_never_reaches_past_it() {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Index, (0..10).collect());
        let reader = SizedReader::open(io.create_index_reader().await.unwrap())
            .await
            .unwrap();

        assert_eq!(reader.size(), 10);
        assert_eq!(reader.read_up_to(6, 64 * 1024).await.unwrap(), [6, 7, 8, 9]);
        assert_eq!(reader.read_up_to(10, 4).await.unwrap(), Vec::<u8>::new());
        assert_eq!(reader.read_up_to(99, 4).await.unwrap(), Vec::<u8>::new());
        assert_eq!(file.read_ats(), 1);

        assert_eq!(reader.read_exact(2, 3).await.unwrap(), [2, 3, 4]);
        assert!(matches!(
            reader.read_exact(8, 3).await,
            Err(ArchiveError::Corrupted(_))
        ));
        assert!(matches!(
            reader.read_exact(u64::MAX, 3).await,
            Err(ArchiveError::Corrupted(_))
        ));

        assert_eq!(file.read_ats(), 2);
        assert_eq!(file.eof_reads(), 0);
    }
}
