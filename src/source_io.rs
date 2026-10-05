use std::io::SeekFrom;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

use crate::ArchiveError;

/// Up to `len` bytes from `offset`, fewer only at the end of the file. The source is left positioned
/// right after what was returned.
pub(crate) async fn read_up_to<TSource: AsyncRead + AsyncSeek + Unpin>(
    source: &mut TSource,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ArchiveError> {
    source.seek(SeekFrom::Start(offset)).await?;

    let mut bytes = vec![0u8; len];
    let mut filled = 0;

    while filled < len {
        let read = source.read(&mut bytes[filled..]).await?;

        if read == 0 {
            break;
        }

        filled += read;
    }

    bytes.truncate(filled);
    Ok(bytes)
}

/// Exactly `bytes.len()` bytes from `offset`; a short file is an `Io` error.
pub(crate) async fn read_exact_at<TSource: AsyncRead + AsyncSeek + Unpin>(
    source: &mut TSource,
    offset: u64,
    bytes: &mut [u8],
) -> Result<(), ArchiveError> {
    source.seek(SeekFrom::Start(offset)).await?;
    source.read_exact(bytes).await?;
    Ok(())
}

/// A page whose header states its own size: `guess` bytes in one request, then the rest if `size_of`
/// (handed at least `header_len` bytes) says the page is longer — one request for the usual page.
pub(crate) async fn read_self_sized<TSource: AsyncRead + AsyncSeek + Unpin>(
    source: &mut TSource,
    offset: u64,
    guess: usize,
    header_len: usize,
    size_of: impl FnOnce(&[u8]) -> Result<usize, ArchiveError>,
) -> Result<Vec<u8>, ArchiveError> {
    let mut bytes = read_up_to(source, offset, guess.max(header_len)).await?;

    if bytes.len() < header_len {
        return Err(ArchiveError::Corrupted(format!(
            "the page at {offset} is past the end of the file"
        )));
    }

    let size = size_of(&bytes)?;

    if size > bytes.len() {
        let mut rest = vec![0u8; size - bytes.len()];
        source.read_exact(&mut rest).await?;
        bytes.extend_from_slice(&rest);
    }

    bytes.truncate(size);
    Ok(bytes)
}
