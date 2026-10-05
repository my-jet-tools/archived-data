use crate::{
    ArchiveError, ArchiveFileReader, DATA_FRAME_LEN, RecordRef, SPLIT_DATA_HEADER_LEN, SizedReader,
    decode_records_frame_header,
};

/// The most one read of a walk asks for.
pub(crate) const MAX_RECORDS_READ_AHEAD: usize = 512 * 1024;

/// One walk's window onto a data file: the bytes of its last read, from the frame that read started at.
///
/// A frame the window holds whole costs no read. On a miss the next read starts at the frame, and its length
/// follows the chain: while each frame starts where the one before it ended — a compact key's run, or a working
/// key whose frames were appended one after another — the window doubles, from one frame up to
/// [`MAX_RECORDS_READ_AHEAD`]; when the chain jumps, it is one frame again. So a compact key of a few frames is
/// one read, the longest day a handful, and a working chain that jumps about the file reads a frame at a time
/// as it did before. The window holds one read at a time, so a walk never keeps more than
/// [`MAX_RECORDS_READ_AHEAD`] bytes of it.
pub(crate) struct RecordsReadAhead {
    window: Option<(u8, u64, Vec<u8>)>,
    read_len: usize,
    last_end: Option<(u8, u64)>,
}

impl RecordsReadAhead {
    pub fn new() -> Self {
        Self {
            window: None,
            read_len: DATA_FRAME_LEN,
            last_end: None,
        }
    }

    /// The frame `at` names, from `file`: its header and its records, the room after them dropped. A frame
    /// whose size reaches past its file, or whose records reach past its size, is `Corrupted`.
    pub async fn frame<TReader: ArchiveFileReader>(
        &mut self,
        file: &SizedReader<TReader>,
        at: RecordRef,
    ) -> Result<Vec<u8>, ArchiveError> {
        if let Some(frame) = self.take_from_window(file, at, false)? {
            return Ok(frame);
        }

        let contiguous = self.last_end == Some((at.file_no, at.frame as u64));

        self.read_len = if contiguous {
            (self.read_len * 2).min(MAX_RECORDS_READ_AHEAD)
        } else {
            DATA_FRAME_LEN
        };

        let bytes = file.read_up_to(at.frame as u64, self.read_len).await?;
        self.window = Some((at.file_no, at.frame as u64, bytes));

        if let Some(frame) = self.take_from_window(file, at, true)? {
            return Ok(frame);
        }

        // Only a frame longer than the read is left: its header is in the window, its records reach past it.
        let (_, _, window) = self.window.as_ref().unwrap();
        let (size, _, used) = decode_records_frame_header(window, at.frame)?;

        let mut frame = window.clone();
        let rest = file
            .read_exact(
                at.frame as u64 + frame.len() as u64,
                SPLIT_DATA_HEADER_LEN + used - frame.len(),
            )
            .await?;
        frame.extend_from_slice(&rest);

        self.passed(at, size);
        Ok(frame)
    }

    /// The walk went through the frame `at` names, of `size` bytes, without this window — out of a cache: the
    /// next frame is contiguous when it starts where this one ends.
    pub fn passed(&mut self, at: RecordRef, size: usize) {
        self.last_end = Some((at.file_no, at.frame as u64 + size as u64));
    }

    /// The frame `at` names out of the window, when the window holds it whole. `fresh` says the window was
    /// just read from this frame, so a header it cannot hold is `Corrupted` rather than a miss.
    fn take_from_window<TReader: ArchiveFileReader>(
        &mut self,
        file: &SizedReader<TReader>,
        at: RecordRef,
        fresh: bool,
    ) -> Result<Option<Vec<u8>>, ArchiveError> {
        let Some((file_no, start, window)) = &self.window else {
            return Ok(None);
        };

        let offset = at.frame as u64;

        if *file_no != at.file_no || offset < *start {
            return Ok(None);
        }

        let within = (offset - start) as usize;

        if !fresh && within + SPLIT_DATA_HEADER_LEN > window.len() {
            return Ok(None);
        }

        let (size, _, used) =
            decode_records_frame_header(&window[within.min(window.len())..], at.frame)?;

        if offset + size as u64 > file.size() {
            return Err(ArchiveError::Corrupted(format!(
                "the data frame at {offset} of data file {} declares {size} bytes, past the end of the \
                 {}-byte file",
                at.file_no,
                file.size()
            )));
        }

        let end = within + SPLIT_DATA_HEADER_LEN + used;

        if end > window.len() {
            return Ok(None);
        }

        let frame = window[within..end].to_vec();
        self.passed(at, size);
        Ok(Some(frame))
    }
}
