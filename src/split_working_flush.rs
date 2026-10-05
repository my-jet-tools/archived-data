use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, L0FrameBody, PreallocatedDataItem,
    PreallocatedSlotKey, SplitDataWriters, SplitIndexWriter, SplitWorkingL0Chain,
    SplitWriterDataPages, SplitWriterL1Pages,
};

/// The flush of a WORKING split archive, the same for every module — only how an L1 page and an L0 body
/// encode is the module's, handed in as `encode_l1` and `l0_bodies`.
///
/// A flush ends a CYCLE: everything written since the last flush that completed. The first byte of a cycle
/// to reach the index or a data file — in `write`, when a page leaves a cache, or here — is preceded by
/// one `write_at` of 8 bytes that spoils frame 0's hash (see [`SplitIndexWriter::spoil_frame_0`]); from it
/// until this flush completes, the index is refused at open. In order:
///
/// 1. every dirty L1 page reaches the index, in offset order;
/// 2. the index must be appended up to everything reserved, because the continuations are reserved next;
/// 3. every dirty data page reaches its data file;
/// 4. a cycle that wrote nothing ends here, with no write at all: frame 0 still verifies, and what it
///    describes is still everything in the files — L0 changes only with an L1 page, and every L1 page that
///    changed reached the index by step 1;
/// 5. L0 is laid out: the continuations it has are rewritten in place, new ones appended at the end;
/// 6. frame 0 — its entries, then its header with the real hash — is written LAST, and the next cycle
///    starts with a frame 0 that verifies.
///
/// So a working index opens only when its last flush completed AND nothing was written after it. A flush
/// that fails, a flush future dropped at an await, or a process that dies before step 6 ends, once any byte
/// of this cycle reached a file, leaves an index refused as `Corrupted` — never one that opens with fewer
/// keys than were flushed, or with none. A flush that stops before any byte of the cycle reached a file (its
/// spoil failed, or it was dropped before writing) changed nothing: the index opens as the last completed
/// flush left it, because nothing was written after that flush.
pub(crate) async fn flush_split_working_archive<TEntry, TSlot, TItem, TWriter, TReader>(
    index: &mut SplitIndexWriter<TWriter, TReader>,
    l0_chain: &mut SplitWorkingL0Chain,
    l1_pages: &mut SplitWriterL1Pages<TEntry>,
    encode_l1: &impl Fn(&[TEntry]) -> Result<Vec<u8>, ArchiveError>,
    data_pages: &mut SplitWriterDataPages<TSlot, TItem>,
    data_files: &mut SplitDataWriters<TWriter>,
    l0_bodies: Vec<L0FrameBody>,
) -> Result<(), ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TWriter: ArchiveFileWriter,
    TReader: ArchiveFileReader,
{
    l1_pages.flush(index, encode_l1).await?;

    if index.appended() != index.reserved() {
        return Err(ArchiveError::ContractViolation(format!(
            "the index is reserved up to {} and appended only up to {}: \
             the writer is not usable after an earlier error",
            index.reserved(),
            index.appended()
        )));
    }

    data_pages.flush(data_files, index).await?;

    if !index.is_frame_0_spoiled() {
        return Ok(());
    }

    let writes = l0_chain.lay_out(l0_bodies, |len| index.reserve(len))?;

    for write in writes {
        if write.append {
            index.append(write.offset, &write.bytes).await?;
        } else {
            index.write_at(write.offset, &write.bytes).await?;
        }
    }

    index.frame_0_verifies(l0_chain.invalidation());
    Ok(())
}
