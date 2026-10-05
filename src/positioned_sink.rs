use std::io::SeekFrom;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::ArchiveError;

/// A writer's sink, remembering where it stands so consecutive writes need no seek. A failed call
/// forgets the position, so the next one seeks again.
pub(crate) struct PositionedSink<TSink> {
    sink: TSink,
    position: Option<u64>,
}

impl<TSink: AsyncRead + AsyncWrite + AsyncSeek + Unpin> PositionedSink<TSink> {
    pub fn new(sink: TSink) -> Self {
        Self {
            sink,
            position: None,
        }
    }

    pub async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), ArchiveError> {
        self.seek_to(offset).await?;

        if let Err(err) = self.sink.write_all(bytes).await {
            self.position = None;
            return Err(err.into());
        }

        self.position = Some(offset + bytes.len() as u64);
        Ok(())
    }

    pub async fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), ArchiveError> {
        self.seek_to(offset).await?;

        if let Err(err) = self.sink.read_exact(bytes).await {
            self.position = None;
            return Err(err.into());
        }

        self.position = Some(offset + bytes.len() as u64);
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<(), ArchiveError> {
        self.sink.flush().await?;
        Ok(())
    }

    pub fn into_inner(self) -> TSink {
        self.sink
    }

    async fn seek_to(&mut self, offset: u64) -> Result<(), ArchiveError> {
        if self.position == Some(offset) {
            return Ok(());
        }

        self.position = None;
        self.sink.seek(SeekFrom::Start(offset)).await?;
        self.position = Some(offset);
        Ok(())
    }
}
