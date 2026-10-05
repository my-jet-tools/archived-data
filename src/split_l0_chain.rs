//! The L0 CHAIN of the split index. What an entry looks like is the module's; this file only frames the
//! entries and links the frames, as the crate's `l0_chain` does for the SingleFile format, with `u32`
//! offsets:
//!
//! ```text
//! frame   size u32 · next u32 · count u32 · hash u64    then the entries, zero-padded to size
//! ```
//!
//! Frame 0 is always at index offset 0, and every frame's hash is checked before the frame is used.
//!
//! A WORKING index uses that check to say whether its last flush completed AND nothing was written to its
//! files after it. Frame 0 is created with a hash that does not verify; the first byte that reaches the
//! index or a data file after a completed flush is preceded by the write that spoils it again
//! ([`SplitWorkingL0Chain::invalidation`]); and frame 0 — its entries, then its header with the real hash —
//! is the LAST write of a flush. An index whose writer never completed a flush, stopped halfway through one,
//! or wrote anything after its last one, is refused at open as `Corrupted`: never an archive that holds no
//! keys, never one whose L0 lost the intervals a half-written chain would skip, and never one whose pages
//! its L0 no longer describes.

use crate::{
    ArchiveError, ArchiveFileReader, L0FrameBody, SPLIT_L0_HEADER_LEN, SPLIT_L1_HEADER_LEN,
    SizedReader, SplitFrame, SplitFrameKind, encode_split_l0_frame, le_u32, read_split_chain,
    split_l0_header_hash,
};

/// How much of the index's head is asked for to read frame 0 in one request before its header is known.
const SPLIT_L0_READ_GUESS: usize = 64 * 1024;

/// Where the hash of an L0 header starts: after `size`, `next` and `count`.
const L0_HASH_AT: usize = SPLIT_L0_HEADER_LEN - 8;

/// One write of a laid-out chain. `append` marks a continuation reserved by this lay-out: its bytes are
/// not in the index yet, and are appended in the order the writes come. Every other write overwrites bytes
/// the index holds already.
pub(crate) struct SplitL0FrameWrite {
    pub offset: u32,
    pub append: bool,
    pub bytes: Vec<u8>,
}

/// The frames of a WORKING index's L0: frame 0 is appended at offset 0 when the index is created, and
/// continuations of the same size are reserved at the end of the index the first time L0 outgrows the
/// frames it has. L0 never shrinks, so a continuation once reserved is rewritten in place by every later
/// flush.
pub(crate) struct SplitWorkingL0Chain {
    frame_len: usize,
    continuations: Vec<u32>,
    /// `next` and `count` of frame 0 as the last lay-out wrote them: the fields whose hash
    /// [`Self::invalidation`] spoils.
    frame_0: (u32, u32),
}

impl SplitWorkingL0Chain {
    pub fn new(frame_len: usize) -> Self {
        Self {
            frame_len,
            continuations: Vec::new(),
            frame_0: (0, 0),
        }
    }

    pub fn frames(&self) -> usize {
        1 + self.continuations.len()
    }

    /// Frame 0 of an index that holds nothing, with its real hash.
    pub fn empty_frame(&self) -> Result<Vec<u8>, ArchiveError> {
        encode_split_l0_frame(self.frame_len, 0, 0, &[])
    }

    /// What the writer appends when it creates the index: the empty frame 0 with its hash inverted, so the
    /// index is refused at open as `Corrupted` until a flush completes — never read as an archive that holds
    /// no keys.
    pub fn unflushed_frame(&self) -> Result<Vec<u8>, ArchiveError> {
        let mut frame = self.empty_frame()?;

        for byte in &mut frame[L0_HASH_AT..SPLIT_L0_HEADER_LEN] {
            *byte = !*byte;
        }

        Ok(frame)
    }

    /// The write that spoils frame 0's hash: the hash of the fields the last lay-out wrote, inverted, in
    /// place. The index writer makes it before the first byte that reaches the index or a data file after a
    /// completed flush (see [`crate::SplitIndexWriter::spoil_frame_0`]); from it until frame 0's header is
    /// written again, the index is refused at open. Before any lay-out it is the unflushed frame's hash.
    pub fn invalidation(&self) -> SplitL0FrameWrite {
        let (next, count) = self.frame_0;
        let hash = split_l0_header_hash(self.frame_len as u32, next, count);

        SplitL0FrameWrite {
            offset: L0_HASH_AT as u32,
            append: false,
            bytes: (!hash).to_le_bytes().to_vec(),
        }
    }

    /// The writes that put `bodies` across the chain; no bodies is one empty frame 0. `reserve` hands out
    /// the offset of a new continuation at the end of the index.
    ///
    /// Every continuation comes first, in chain order — rewritten in place, or appended when this lay-out
    /// reserved it — and frame 0 LAST: its entries, then its 20-byte header with the real hash, so the
    /// index verifies again only once every other byte of the flush is in it.
    pub fn lay_out(
        &mut self,
        bodies: Vec<L0FrameBody>,
        mut reserve: impl FnMut(usize) -> Result<u32, ArchiveError>,
    ) -> Result<Vec<SplitL0FrameWrite>, ArchiveError> {
        let needed = bodies.len().max(1);
        let known = self.frames();

        let mut reserved = Vec::new();
        while known + reserved.len() < needed {
            reserved.push(reserve(self.frame_len)?);
        }
        self.continuations.extend(reserved);

        let offsets: Vec<u32> = std::iter::once(0)
            .chain(self.continuations.iter().copied())
            .take(needed)
            .collect();

        let mut frames = Vec::with_capacity(needed);

        if bodies.is_empty() {
            frames.push(self.empty_frame()?);
        }

        for (index, frame) in bodies.iter().enumerate() {
            let next = offsets.get(index + 1).copied().unwrap_or(0);
            frames.push(encode_split_l0_frame(
                self.frame_len,
                next,
                frame.count,
                &frame.body,
            )?);
        }

        let mut frames = frames.into_iter();
        let mut frame_0 = frames.next().expect("a lay-out has frame 0");

        let mut writes: Vec<SplitL0FrameWrite> = frames
            .enumerate()
            .map(|(at, bytes)| SplitL0FrameWrite {
                offset: offsets[at + 1],
                append: at + 1 >= known,
                bytes,
            })
            .collect();

        let entries = frame_0.split_off(SPLIT_L0_HEADER_LEN);

        if !entries.is_empty() {
            writes.push(SplitL0FrameWrite {
                offset: SPLIT_L0_HEADER_LEN as u32,
                append: false,
                bytes: entries,
            });
        }

        self.frame_0 = (le_u32(&frame_0, 4), le_u32(&frame_0, 8));

        writes.push(SplitL0FrameWrite {
            offset: 0,
            append: false,
            bytes: frame_0,
        });

        Ok(writes)
    }
}

/// Every frame of the index's L0 chain, in chain order.
pub(crate) async fn read_split_l0_chain<TReader: ArchiveFileReader>(
    index: &SizedReader<TReader>,
) -> Result<Vec<SplitFrame>, ArchiveError> {
    read_split_chain(index, 0, SplitFrameKind::L0, SPLIT_L0_READ_GUESS).await
}

/// Refuses an L0 that names an L1 page whose header does not fit inside the index, with no read: every
/// `l1_offset + 12` must be at most `index_size`.
///
/// A writer dropped halfway through the index still has its file finished by `Drop`, so an index can end
/// right after a well-formed L0; this is what refuses it at open instead of at the first key. An index cut
/// INSIDE its last L1 page passes it, and is refused by `refuse_an_index_cut_inside_its_last_l1_page`.
pub(crate) fn refuse_l1_offsets_past_the_index(
    l1_offsets: impl IntoIterator<Item = u32>,
    index_size: u64,
) -> Result<(), ArchiveError> {
    for offset in l1_offsets {
        if offset as u64 + SPLIT_L1_HEADER_LEN as u64 > index_size {
            return Err(ArchiveError::Corrupted(format!(
                "L0 names an L1 page at {offset}, past the end of a {index_size}-byte index"
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo};
    use crate::{
        ArchiveError, IoReaderFactory, L0FrameBody, SizedReader, SplitL0FrameWrite,
        SplitWorkingL0Chain, l0_frame, patch_u32, read_split_l0_chain,
        refuse_l1_offsets_past_the_index,
    };

    fn bodies(counts: &[usize]) -> Vec<L0FrameBody> {
        counts
            .iter()
            .map(|count| L0FrameBody {
                count: *count,
                body: vec![*count as u8; *count],
            })
            .collect()
    }

    /// Hands out offsets from `end`, as an index writer's reservation would, and records the lengths.
    fn reserver(
        end: &mut u32,
        lens: &mut Vec<usize>,
    ) -> impl FnMut(usize) -> Result<u32, ArchiveError> {
        move |len| {
            lens.push(len);
            let offset = *end;
            *end += len as u32;
            Ok(offset)
        }
    }

    /// `(offset, append, len)` of every write, in order.
    fn summary(writes: &[SplitL0FrameWrite]) -> Vec<(u32, bool, usize)> {
        writes
            .iter()
            .map(|write| (write.offset, write.append, write.bytes.len()))
            .collect()
    }

    /// Applies writes to an index as the split index writer does: an append at the end, anything else in
    /// place over bytes the index holds.
    fn apply(index: &mut Vec<u8>, writes: &[SplitL0FrameWrite]) {
        for write in writes {
            let at = write.offset as usize;

            if write.append {
                assert_eq!(at, index.len(), "an append at the end");
                index.extend_from_slice(&write.bytes);
            } else {
                index[at..at + write.bytes.len()].copy_from_slice(&write.bytes);
            }
        }
    }

    async fn read_back(index: Vec<u8>) -> Result<Vec<(u32, Vec<u8>)>, ArchiveError> {
        let io = InMemoryIo::default();
        io.put(FileRole::Index, index);
        let reader = SizedReader::open(io.create_index_reader().await.unwrap())
            .await
            .unwrap();

        let frames = read_split_l0_chain(&reader).await?;

        Ok(frames
            .iter()
            .map(|frame| {
                (
                    frame.header.field,
                    frame.body[..frame.header.field as usize].to_vec(),
                )
            })
            .collect())
    }

    #[test]
    fn no_bodies_is_one_empty_frame_0() {
        let mut chain = SplitWorkingL0Chain::new(28);
        let (mut end, mut lens) = (500, Vec::new());

        let writes = chain
            .lay_out(Vec::new(), reserver(&mut end, &mut lens))
            .unwrap();

        // Frame 0's entries, then its header.
        assert_eq!(summary(&writes), [(20, false, 8), (0, false, 20)]);

        let mut index = chain.unflushed_frame().unwrap();
        apply(&mut index, &writes);
        assert_eq!(index, l0_frame(28, 0, 0, &[]));
        assert_eq!(index, chain.empty_frame().unwrap());
        assert!(lens.is_empty());
        assert_eq!(chain.frames(), 1);
    }

    #[test]
    fn only_the_continuations_a_lay_out_reserves_are_appended() {
        let mut chain = SplitWorkingL0Chain::new(28);
        let (mut end, mut lens) = (500, Vec::new());

        let writes = chain
            .lay_out(bodies(&[2, 2, 1]), reserver(&mut end, &mut lens))
            .unwrap();
        assert_eq!(
            summary(&writes),
            [
                (500, true, 28),
                (528, true, 28),
                (20, false, 8),
                (0, false, 20)
            ]
        );
        assert_eq!(writes[0].bytes, l0_frame(28, 528, 2, &[2, 2]));
        assert_eq!(writes[1].bytes, l0_frame(28, 0, 1, &[1]));
        assert_eq!(
            [&writes[3].bytes[..], &writes[2].bytes[..]].concat(),
            l0_frame(28, 500, 2, &[2, 2])
        );
        assert_eq!(lens, [28, 28]);

        let writes = chain
            .lay_out(bodies(&[2, 2, 2]), reserver(&mut end, &mut lens))
            .unwrap();
        assert_eq!(
            summary(&writes),
            [
                (500, false, 28),
                (528, false, 28),
                (20, false, 8),
                (0, false, 20)
            ]
        );

        let writes = chain
            .lay_out(bodies(&[2, 2, 2, 1]), reserver(&mut end, &mut lens))
            .unwrap();
        assert_eq!(
            summary(&writes),
            [
                (500, false, 28),
                (528, false, 28),
                (556, true, 28),
                (20, false, 8),
                (0, false, 20)
            ]
        );
        assert_eq!(writes[1].bytes, l0_frame(28, 556, 2, &[2, 2]));
        assert_eq!(chain.frames(), 4);
    }

    #[test]
    fn a_refused_reservation_records_no_continuation() {
        let mut chain = SplitWorkingL0Chain::new(28);

        let refused = chain.lay_out(bodies(&[1, 1]), |_| {
            Err(ArchiveError::ContractViolation("full".to_string()))
        });

        assert!(matches!(refused, Err(ArchiveError::ContractViolation(_))));
        assert_eq!(chain.frames(), 1);
    }

    fn refuses_the_hash(result: Result<Vec<(u32, Vec<u8>)>, ArchiveError>, case: &str) {
        match result {
            Err(ArchiveError::Corrupted(message)) => {
                assert!(
                    message.contains("fails its header hash"),
                    "{case}: {message}"
                )
            }
            other => panic!("{case}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_index_verifies_only_once_a_whole_flush_is_in_it() {
        let mut chain = SplitWorkingL0Chain::new(28);
        let (mut end, mut lens) = (28, Vec::new());

        let mut index = chain.unflushed_frame().unwrap();
        refuses_the_hash(read_back(index.clone()).await, "never flushed");

        // The first flush: the invalidation, then every prefix of the lay-out short of frame 0's header.
        let invalidation = chain.invalidation();
        assert_eq!((invalidation.offset, invalidation.append), (12, false));
        apply(&mut index, &[invalidation]);
        assert_eq!(index, chain.unflushed_frame().unwrap());

        let writes = chain
            .lay_out(bodies(&[3, 1]), reserver(&mut end, &mut lens))
            .unwrap();

        for done in 0..writes.len() {
            let mut partial = index.clone();
            apply(&mut partial, &writes[..done]);
            refuses_the_hash(read_back(partial).await, &format!("{done} writes of 1"));
        }

        apply(&mut index, &writes);
        assert_eq!(
            read_back(index.clone()).await.unwrap(),
            [(3, vec![3, 3, 3]), (1, vec![1])]
        );

        // A later flush spoils the hash of what the index holds now, and restores it only at its end.
        let invalidation = chain.invalidation();
        apply(&mut index, &[invalidation]);
        refuses_the_hash(read_back(index.clone()).await, "invalidated");

        let writes = chain
            .lay_out(bodies(&[2, 2]), reserver(&mut end, &mut lens))
            .unwrap();

        for done in 0..writes.len() {
            let mut partial = index.clone();
            apply(&mut partial, &writes[..done]);
            refuses_the_hash(read_back(partial).await, &format!("{done} writes of 2"));
        }

        apply(&mut index, &writes);
        assert_eq!(
            read_back(index.clone()).await.unwrap(),
            [(2, vec![2, 2]), (2, vec![2, 2])]
        );

        // An invalidation on a chain that laid out nothing yet still spoils the unflushed frame's hash.
        let fresh = SplitWorkingL0Chain::new(28);
        let mut empty = fresh.empty_frame().unwrap();
        apply(&mut empty, &[fresh.invalidation()]);
        refuses_the_hash(read_back(empty).await, "an empty frame invalidated");
    }

    #[tokio::test]
    async fn a_laid_out_chain_reads_back_in_one_clamped_read_per_frame() {
        let mut chain = SplitWorkingL0Chain::new(28);
        let (mut end, mut lens) = (28, Vec::new());
        let writes = chain
            .lay_out(bodies(&[3, 1]), reserver(&mut end, &mut lens))
            .unwrap();

        let mut index = chain.unflushed_frame().unwrap();
        apply(&mut index, &writes);

        let io = InMemoryIo::default();
        let file = io.put(FileRole::Index, index);
        let reader = SizedReader::open(io.create_index_reader().await.unwrap())
            .await
            .unwrap();

        let frames = read_split_l0_chain(&reader).await.unwrap();

        let seen: Vec<(u32, Vec<u8>)> = frames
            .iter()
            .map(|frame| {
                (
                    frame.header.field,
                    frame.body[..frame.header.field as usize].to_vec(),
                )
            })
            .collect();
        assert_eq!(seen, [(3, vec![3, 3, 3]), (1, vec![1])]);
        assert_eq!(file.read_ats(), 2);
        assert_eq!(file.eof_reads(), 0);
    }

    #[tokio::test]
    async fn a_continuation_whose_hash_fails_refuses_the_whole_chain() {
        let mut index = l0_frame(28, 28, 1, &[5]);
        index.extend_from_slice(&l0_frame(28, 0, 2, &[6, 6]));
        assert_eq!(
            read_back(index.clone()).await.unwrap(),
            [(1, vec![5]), (2, vec![6, 6])]
        );

        for at in [28 + 8, 28 + 12, 28 + 19] {
            let mut damaged = index.clone();
            damaged[at] ^= 0x10;

            match read_back(damaged).await {
                Err(ArchiveError::Corrupted(message)) => {
                    assert!(message.contains("L0 frame at 28"), "{message}");
                    assert!(message.contains("not a split archive index"), "{message}");
                }
                other => panic!("byte {at}: {other:?}"),
            }
        }

        // A continuation patched to a count its body could hold, but without its hash rewritten.
        let mut recounted = index.clone();
        patch_u32(&mut recounted, 28 + 8, 1);
        assert!(matches!(
            read_back(recounted).await,
            Err(ArchiveError::Corrupted(_))
        ));
    }

    #[test]
    fn an_l1_offset_must_leave_room_for_an_l1_header_inside_the_index() {
        refuse_l1_offsets_past_the_index([], 0).unwrap();
        refuse_l1_offsets_past_the_index([20, 88], 100).unwrap();

        for (offsets, size) in [(vec![20, 89], 100), (vec![100], 100), (vec![u32::MAX], 100)] {
            assert!(matches!(
                refuse_l1_offsets_past_the_index(offsets, size),
                Err(ArchiveError::Corrupted(_))
            ));
        }
    }
}
