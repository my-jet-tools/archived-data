use crate::ArchiveFileReader;

/// A file already in memory, read through the same trait as one that is not, so a global day index held as
/// bytes is parsed by the very reader — and checked by the very checks — a file in S3 is.
pub(crate) struct BytesFileReader {
    bytes: Vec<u8>,
}

impl BytesFileReader {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl ArchiveFileReader for BytesFileReader {
    async fn read_at(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(len)?))
            .filter(|range| range.end <= self.bytes.len());

        match range {
            Some(range) => Ok(self.bytes[range].to_vec()),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "{len} bytes at {offset} reach past the end of {} bytes",
                    self.bytes.len()
                ),
            )),
        }
    }

    async fn size(&self) -> std::io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
}
