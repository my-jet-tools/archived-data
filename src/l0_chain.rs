//! The L0 CHAIN framing every module of this crate shares. What an entry looks like is the module's;
//! this file only frames the entries and links the frames.
//!
//! ```text
//! frame   size u32 · next u64 · count u32    then the entries, zero-padded to size
//! ```
//!
//! Frame 0 is always at offset 0. `next` is the absolute offset of the continuation frame, 0 on the
//! last one. A continuation always lies past the end of the frame pointing at it — continuations are
//! appended at the end of the file — so a `next` pointing into frame 0, back into an earlier frame, or
//! in a cycle is refused, and the walk ends by construction.

use tokio::io::{AsyncRead, AsyncSeek};

use crate::{ArchiveError, le_u32, le_u64, read_up_to};

pub(crate) const L0_HEADER_LEN: usize = 16;

/// How much of the file's head is asked for to read frame 0 in one request before its header is known.
const L0_READ_GUESS: usize = 64 * 1024;

/// One frame as the file holds it: `body` is everything past the header, up to the frame's `size`
/// (the padding included), and the module decodes `count` entries out of it.
pub(crate) struct L0FrameRead {
    pub offset: u64,
    pub count: usize,
    pub body: Vec<u8>,
}

/// The entries of one frame, already encoded by the module.
pub(crate) struct L0FrameBody {
    pub count: usize,
    pub body: Vec<u8>,
}

/// A frame of `size` bytes: header, `body`, zeros. A compact file passes
/// `size == L0_HEADER_LEN + body.len()`.
pub(crate) fn encode_l0_frame(
    size: usize,
    next: u64,
    count: usize,
    body: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    if L0_HEADER_LEN + body.len() > size {
        return Err(ArchiveError::ContractViolation(format!(
            "an L0 frame of {size} bytes cannot hold {} bytes of entries",
            body.len()
        )));
    }

    let (Ok(size_field), Ok(count_field)) = (u32::try_from(size), u32::try_from(count)) else {
        return Err(ArchiveError::ContractViolation(format!(
            "an L0 frame of {size} bytes and {count} entries does not fit its u32 fields"
        )));
    };

    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&size_field.to_le_bytes());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&count_field.to_le_bytes());
    out.extend_from_slice(body);
    out.resize(size, 0);
    Ok(out)
}

/// Every frame of the chain, in chain order.
pub(crate) async fn read_l0_chain<TSource: AsyncRead + AsyncSeek + Unpin>(
    source: &mut TSource,
) -> Result<Vec<L0FrameRead>, ArchiveError> {
    let mut frames = Vec::new();
    let mut offset = 0u64;
    let mut guess = L0_READ_GUESS;

    loop {
        let (frame, next) = read_frame(source, offset, guess).await?;
        guess = L0_HEADER_LEN + frame.body.len();
        frames.push(frame);

        if next == 0 {
            return Ok(frames);
        }

        let end = offset + guess as u64;

        if next < end {
            return Err(ArchiveError::Corrupted(format!(
                "the L0 frame at {offset} chains to {next}, which is not past its end {end}"
            )));
        }

        offset = next;
    }
}

async fn read_frame<TSource: AsyncRead + AsyncSeek + Unpin>(
    source: &mut TSource,
    offset: u64,
    guess: usize,
) -> Result<(L0FrameRead, u64), ArchiveError> {
    let mut bytes = read_up_to(source, offset, guess.max(L0_HEADER_LEN)).await?;

    if bytes.len() < L0_HEADER_LEN {
        return Err(ArchiveError::Corrupted(format!(
            "the L0 frame at {offset} is past the end of the file ({} header bytes)",
            bytes.len()
        )));
    }

    let size = le_u32(&bytes, 0) as usize;
    let next = le_u64(&bytes, 4);
    let count = le_u32(&bytes, 12) as usize;

    if size < L0_HEADER_LEN {
        return Err(ArchiveError::Corrupted(format!(
            "the L0 frame at {offset} declares {size} bytes, shorter than its header"
        )));
    }

    if size > bytes.len() {
        let rest = read_up_to(source, offset + bytes.len() as u64, size - bytes.len()).await?;
        bytes.extend_from_slice(&rest);
    }

    if bytes.len() < size {
        return Err(ArchiveError::Corrupted(format!(
            "the L0 frame at {offset} declares {size} bytes and is cut short"
        )));
    }

    bytes.truncate(size);
    bytes.drain(..L0_HEADER_LEN);

    Ok((
        L0FrameRead {
            offset,
            count,
            body: bytes,
        },
        next,
    ))
}

/// The frames of a WORKING file's L0: frame 0 reserved at offset 0, continuations of the same size
/// allocated at the end of the file the first time L0 outgrows the frames it has. L0 never shrinks, so
/// a continuation once allocated is used by every later flush.
pub(crate) struct WorkingL0Chain {
    frame_len: usize,
    continuations: Vec<u64>,
}

impl WorkingL0Chain {
    pub fn new(frame_len: usize) -> Self {
        Self {
            frame_len,
            continuations: Vec::new(),
        }
    }

    pub fn frames(&self) -> usize {
        1 + self.continuations.len()
    }

    /// The `(offset, bytes)` writes that put `bodies` across the chain, frame 0 first. No bodies is one
    /// empty frame 0. `allocate` hands out the offset of a new continuation at the end of the file.
    pub fn lay_out(
        &mut self,
        bodies: Vec<L0FrameBody>,
        mut allocate: impl FnMut(usize) -> u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, ArchiveError> {
        let needed = bodies.len().max(1);

        while self.frames() < needed {
            self.continuations.push(allocate(self.frame_len));
        }

        let offsets: Vec<u64> = std::iter::once(0)
            .chain(self.continuations.iter().copied())
            .take(needed)
            .collect();

        if bodies.is_empty() {
            return Ok(vec![(0, encode_l0_frame(self.frame_len, 0, 0, &[])?)]);
        }

        bodies
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let next = offsets.get(index + 1).copied().unwrap_or(0);
                let bytes = encode_l0_frame(self.frame_len, next, frame.count, &frame.body)?;
                Ok((offsets[index], bytes))
            })
            .collect()
    }
}
