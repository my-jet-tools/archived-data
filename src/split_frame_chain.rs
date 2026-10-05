//! Reading a frame and every continuation chained to it. A continuation always lies past the end of the
//! frame pointing at it, because writers only append, so a `next` into the frame itself, back into an
//! earlier frame, or past the end of the file is `Corrupted`, and a walk ends by construction.

use crate::{
    ArchiveError, ArchiveFileReader, SizedReader, SplitFrame, SplitFrameHeader, SplitFrameKind,
};

/// Every frame of the chain starting at `offset`, in chain order. A frame no longer than `guess` costs
/// one `read_at`; a continuation is guessed as long as the frame before it.
pub(crate) async fn read_split_chain<TReader: ArchiveFileReader>(
    reader: &SizedReader<TReader>,
    offset: u64,
    kind: SplitFrameKind,
    guess: usize,
) -> Result<Vec<SplitFrame>, ArchiveError> {
    let mut frames = Vec::new();
    let mut offset = offset;
    let mut guess = guess;

    loop {
        let frame = read_split_frame(reader, offset, kind, guess).await?;
        let next = next_frame(&frame.header, reader.size())?;
        guess = frame.header.size;
        frames.push(frame);

        match next {
            Some(next) => offset = next,
            None => return Ok(frames),
        }
    }
}

/// The headers alone of the chain starting at `offset`: one `read_at` of a header per frame.
pub(crate) async fn read_split_header_chain<TReader: ArchiveFileReader>(
    reader: &SizedReader<TReader>,
    offset: u64,
    kind: SplitFrameKind,
) -> Result<Vec<SplitFrameHeader>, ArchiveError> {
    let mut headers = Vec::new();
    let mut offset = offset;

    loop {
        let bytes = reader.read_up_to(offset, kind.header_len()).await?;
        let header = SplitFrameHeader::decode(&bytes, offset, kind)?;
        check_within_file(&header, reader.size())?;
        let next = next_frame(&header, reader.size())?;
        headers.push(header);

        match next {
            Some(next) => offset = next,
            None => return Ok(headers),
        }
    }
}

async fn read_split_frame<TReader: ArchiveFileReader>(
    reader: &SizedReader<TReader>,
    offset: u64,
    kind: SplitFrameKind,
    guess: usize,
) -> Result<SplitFrame, ArchiveError> {
    let header_len = kind.header_len();
    let mut bytes = reader.read_up_to(offset, guess.max(header_len)).await?;

    let header = SplitFrameHeader::decode(&bytes, offset, kind)?;
    check_within_file(&header, reader.size())?;

    if header.size > bytes.len() {
        let rest = reader
            .read_exact(offset + bytes.len() as u64, header.size - bytes.len())
            .await?;
        bytes.extend_from_slice(&rest);
    }

    bytes.truncate(header.size);
    bytes.drain(..header_len);

    Ok(SplitFrame {
        header,
        body: bytes,
    })
}

fn check_within_file(header: &SplitFrameHeader, file_size: u64) -> Result<(), ArchiveError> {
    if header.end() > file_size {
        return Err(ArchiveError::Corrupted(format!(
            "the {} at {} declares {} bytes, past the end of the {file_size}-byte file",
            header.kind, header.offset, header.size
        )));
    }

    Ok(())
}

fn next_frame(header: &SplitFrameHeader, file_size: u64) -> Result<Option<u64>, ArchiveError> {
    let next = header.next as u64;

    if next == 0 {
        return Ok(None);
    }

    if next < header.end() {
        return Err(ArchiveError::Corrupted(format!(
            "the {} at {} chains to {next}, which is not past its end {}",
            header.kind,
            header.offset,
            header.end()
        )));
    }

    if next >= file_size {
        return Err(ArchiveError::Corrupted(format!(
            "the {} at {} chains to {next}, past the end of the {file_size}-byte file",
            header.kind, header.offset
        )));
    }

    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::in_memory_io::{FileRole, InMemoryFile, InMemoryIo, InMemoryReader};
    use crate::{IoReaderFactory, data_frame, l0_frame, l1_frame, patch_u32, restamp_l0};

    async fn open(bytes: Vec<u8>) -> (SizedReader<InMemoryReader>, Arc<InMemoryFile>) {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Index, bytes);
        let reader = SizedReader::open(io.create_index_reader().await.unwrap())
            .await
            .unwrap();
        (reader, file)
    }

    /// Frames at 0, 50 and 100 with filler between them: frames need not be adjacent.
    fn three_frame_chain() -> Vec<u8> {
        let mut bytes = l1_frame(24, 50, 1, &[1; 12]);
        bytes.resize(50, 0xCC);
        bytes.extend_from_slice(&l1_frame(30, 100, 2, &[2; 18]));
        bytes.resize(100, 0xCC);
        bytes.extend_from_slice(&l1_frame(14, 0, 3, &[3; 2]));
        bytes
    }

    fn is_corrupted<T>(result: &Result<T, ArchiveError>) -> bool {
        matches!(result, Err(ArchiveError::Corrupted(_)))
    }

    #[tokio::test]
    async fn a_frame_no_longer_than_the_guess_costs_one_read_at() {
        let mut bytes = l1_frame(40, 0, 2, &[5; 28]);
        bytes.resize(100, 0);
        let (reader, file) = open(bytes).await;

        let frames = read_split_chain(&reader, 0, SplitFrameKind::L1, 64)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header.field, 2);
        assert_eq!(frames[0].body, [5; 28]);
        assert_eq!(file.read_ats(), 1);

        let longer = read_split_chain(&reader, 0, SplitFrameKind::L1, 16)
            .await
            .unwrap();
        assert_eq!(longer[0].body, [5; 28]);
        assert_eq!(file.read_ats(), 3);
    }

    #[tokio::test]
    async fn a_guess_past_the_end_of_the_file_is_clamped() {
        let mut bytes = vec![0xCC; 10];
        bytes.extend_from_slice(&data_frame(22, 0, 4, &[1; 12]));
        let (reader, file) = open(bytes).await;

        let frames = read_split_chain(&reader, 10, SplitFrameKind::Data, 64 * 1024)
            .await
            .unwrap();

        assert_eq!(frames[0].body, [1; 12]);
        assert_eq!(file.read_ats(), 1);
        assert_eq!(file.eof_reads(), 0);
    }

    #[tokio::test]
    async fn a_size_shorter_than_the_header_or_past_the_end_is_corrupted() {
        let (reader, _) = open(l0_frame(8, 0, 0, &[])).await;
        assert!(is_corrupted(
            &read_split_chain(&reader, 0, SplitFrameKind::L0, 64).await
        ));

        let (reader, _) = open(vec![1, 2, 3, 4, 5]).await;
        assert!(is_corrupted(
            &read_split_chain(&reader, 0, SplitFrameKind::L0, 64).await
        ));

        let mut bytes = l0_frame(30, 0, 0, &[]);
        patch_u32(&mut bytes, 0, 100);
        restamp_l0(&mut bytes, 0);
        let (reader, file) = open(bytes).await;

        assert!(is_corrupted(
            &read_split_chain(&reader, 0, SplitFrameKind::L0, 12).await
        ));
        assert!(is_corrupted(
            &read_split_header_chain(&reader, 0, SplitFrameKind::L0).await
        ));
        assert_eq!(file.read_ats(), 2);
        assert_eq!(file.eof_reads(), 0);
    }

    #[tokio::test]
    async fn a_next_not_past_its_frame_or_past_the_file_is_corrupted() {
        let (reader, _) = open(three_frame_chain()).await;
        assert_eq!(
            read_split_chain(&reader, 0, SplitFrameKind::L1, 64)
                .await
                .unwrap()
                .len(),
            3
        );

        // (which frame's next, pointing where): into frame 0, into itself, backwards, past the end.
        let cases = [
            (0, 4),
            (100, 4),
            (50, 50),
            (100, 60),
            (100, 50),
            (50, 114),
            (50, 9999),
        ];

        for (frame, next) in cases {
            let mut bytes = three_frame_chain();
            patch_u32(&mut bytes, frame + 4, next);
            let (reader, _) = open(bytes).await;

            assert!(
                is_corrupted(&read_split_chain(&reader, 0, SplitFrameKind::L1, 64).await),
                "next {next} of the frame at {frame}"
            );
            assert!(
                is_corrupted(&read_split_header_chain(&reader, 0, SplitFrameKind::L1).await),
                "next {next} of the frame at {frame}, headers only"
            );
        }
    }

    #[tokio::test]
    async fn a_chain_comes_back_in_chain_order() {
        let (reader, _) = open(three_frame_chain()).await;

        let frames = read_split_chain(&reader, 0, SplitFrameKind::L1, 64)
            .await
            .unwrap();

        let seen: Vec<(u64, u32, Vec<u8>)> = frames
            .into_iter()
            .map(|frame| (frame.header.offset, frame.header.field, frame.body))
            .collect();

        assert_eq!(
            seen,
            [
                (0, 1, vec![1; 12]),
                (50, 2, vec![2; 18]),
                (100, 3, vec![3; 2])
            ]
        );
    }

    #[tokio::test]
    async fn a_header_chain_reads_only_the_headers() {
        let (reader, file) = open(three_frame_chain()).await;

        let headers = read_split_header_chain(&reader, 0, SplitFrameKind::L1)
            .await
            .unwrap();

        let seen: Vec<(u64, usize, u32)> = headers
            .iter()
            .map(|header| (header.offset, header.size, header.field))
            .collect();

        assert_eq!(seen, [(0, 24, 1), (50, 30, 2), (100, 14, 3)]);
        assert_eq!(file.read_ats(), 3);
        assert_eq!(file.bytes_read(), 36);
    }
}
