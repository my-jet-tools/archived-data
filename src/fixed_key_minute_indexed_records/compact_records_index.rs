use crate::{
    AppendFrontier, ArchiveError, ArchiveFileWriter, COMPACT_L1_CAPACITY, IoWriterFactory,
    MinuteCells, MinuteIndexedRecordsCompactionPlan, PlannedRecordsKey, RecordRef,
    compact_l1_page_len, encode_split_l0_frame, encode_split_l1_frame,
};

/// The compact index, from the plan alone: ONE L0 frame of exact size, an L2 frame per key in key order, then
/// the L1 pages. Append only, and finished once it is exactly its planned size.
pub(crate) async fn write_compact_records_index<TOut, TWriter>(
    plan: &MinuteIndexedRecordsCompactionPlan,
    out: &TOut,
) -> Result<(), ArchiveError>
where
    TOut: IoWriterFactory<TWriter>,
    TWriter: ArchiveFileWriter,
{
    let writer = out.create_index_writer().await?;
    let mut frontier = AppendFrontier::new(0);
    let pages: Vec<&[PlannedRecordsKey]> = plan.keys.chunks(COMPACT_L1_CAPACITY).collect();

    let mut l0 = Vec::new();

    for (page, keys) in pages.iter().enumerate() {
        l0.extend_from_slice(&keys[0].key);
        l0.extend_from_slice(&keys[keys.len() - 1].key);
        l0.extend_from_slice(&plan.l1_offset(page).to_le_bytes());
    }

    let l0_frame = encode_split_l0_frame(plan.l0_len() as usize, 0, pages.len(), &l0)?;
    append(&writer, &mut frontier, &l0_frame).await?;

    for planned in &plan.keys {
        let mut cells = MinuteCells::empty();

        for (minute, frame, offset) in &planned.run.cells {
            cells.set(
                *minute as usize,
                RecordRef {
                    file_no: planned.file_no,
                    frame: planned.run_start + frame,
                    offset: *offset,
                },
            );
        }

        append(&writer, &mut frontier, &cells.encode()).await?;
    }

    let page_len = compact_l1_page_len(plan.key_len);

    for (page, keys) in pages.iter().enumerate() {
        let mut body = Vec::with_capacity(page_len);

        for (index, planned) in keys.iter().enumerate() {
            let rank = page * COMPACT_L1_CAPACITY + index;
            body.extend_from_slice(&planned.key);
            body.extend_from_slice(&plan.l2_offset(rank).to_le_bytes());
        }

        let bytes = encode_split_l1_frame(page_len, 0, keys.len(), &body)?;
        append(&writer, &mut frontier, &bytes).await?;
    }

    if frontier.appended() != plan.index_size() {
        return Err(ArchiveError::ContractViolation(format!(
            "a compact index of {} bytes; its plan's is {}",
            frontier.appended(),
            plan.index_size()
        )));
    }

    Ok(writer.finish().await?)
}

async fn append<TWriter: ArchiveFileWriter>(
    writer: &TWriter,
    frontier: &mut AppendFrontier,
    bytes: &[u8],
) -> Result<(), ArchiveError> {
    let offset = frontier.reserve(bytes.len());
    frontier.check_append(offset, bytes.len())?;
    let returned = writer.append(bytes).await?;
    frontier.confirm_append(returned, bytes.len())
}
