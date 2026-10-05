use crate::{
    ArchiveError, ArchiveFileReader, ArchiveFileWriter, CheckedKeyBytes, CompactRecordsCutter,
    CompactRecordsDataFiles, FixedKeyMinuteIndexedRecordsArchiveReader, IoReaderFactory,
    IoWriterFactory, MinuteCells, MinuteIndexedRecordsCompactionPlan, PlannedRecordsKey,
    PrimaryKeyFixedSize, RecordsCursor, place_records_run, refuse_unordered,
    source_changed_while_compacting, write_compact_records_index,
};

/// The COMPACT form of a day archive of records, written in two steps: planned — every record walked once,
/// so the sizes are exact — then written from the plan, the data files first and the index LAST.
impl<TKey, TFactory, TReader> FixedKeyMinuteIndexedRecordsArchiveReader<TKey, TFactory, TReader>
where
    TKey: PrimaryKeyFixedSize,
    TFactory: IoReaderFactory<TReader>,
    TReader: ArchiveFileReader,
{
    /// What [`Self::write_compacted_to`] will write, from one walk over every record of every key, through
    /// none of the caches: where each key's records land, the exact size of the compact index, and the exact
    /// size of every compact data file under the reader's `max_data_file_bytes`. Every record walked is
    /// checked as a read checks it. An archive with no keys plans a 20-byte index and no data files.
    ///
    /// Keys that do not ascend across L1 pages are `Corrupted`; an index past 4 GiB, a key whose records are
    /// longer than a data file, or data needing data file 256 is a `ContractViolation`. Memory: the first
    /// record of every minute of every key that has one, about 12 bytes each.
    pub async fn compaction_plan(
        &self,
    ) -> Result<MinuteIndexedRecordsCompactionPlan, ArchiveError> {
        let max_data_file_bytes = self.max_data_file_bytes();
        let mut keys = Vec::new();
        let mut files = Vec::new();
        let mut previous: Option<TKey> = None;

        for interval in &self.l0 {
            for (key, l2_offset) in self.source_l1_page(interval).await? {
                refuse_unordered(&previous, &key)?;

                let cells = self.source_l2_frame(l2_offset).await?;
                let mut cutter = CompactRecordsCutter::measuring();
                let mut cursor = self.cursor(&cells);

                if let Some(cursor) = &mut cursor {
                    while let Some(record) = cursor.next().await? {
                        cutter.push(record.minute, record.bytes());
                    }
                }

                let (run, _) = cutter.finish();
                let (file_no, run_start) =
                    place_records_run(&mut files, run.len, max_data_file_bytes)?;

                keys.push(PlannedRecordsKey {
                    key: key_bytes(&key)?,
                    file_no,
                    run_start,
                    run,
                });

                previous = Some(key);
            }
        }

        MinuteIndexedRecordsCompactionPlan::new(keys, TKey::LEN, files)
    }

    /// Writes the COMPACT archive of `plan` through `out`: each data file in order, append only, each
    /// finished before the next is created; then the index, append only, finished LAST.
    ///
    /// Data: every key's records in key order as one run of frames back to back, each frame holding records
    /// while the next one fits 32 KiB with no free space after them and chained to the frame right after it,
    /// a key's run whole in one data file. Index: ONE L0 frame of exact size · an L2 frame per key in key
    /// order, its cells pointing into the compact data · full L1 pages of [`crate::COMPACT_L1_CAPACITY`] keys, the
    /// last zero-padded. The bytes are a function of the keys, the records and the plan's
    /// `max_data_file_bytes` only, so a run over the same archive writes identical files.
    ///
    /// The archive is complete when its index exists AND opens. An `Err` in the data pass leaves no index;
    /// one in the index pass can leave a short index, which `open` refuses as `Corrupted`: its L1 pages come
    /// last, so a cut anywhere leaves the page at the highest offset short or missing. The data pass walks
    /// every record again through no cache, and a source that no longer matches the plan is `Corrupted`.
    /// Memory: one key's frame at a time.
    pub async fn write_compacted_to<TOut, TWriter>(
        &self,
        plan: &MinuteIndexedRecordsCompactionPlan,
        out: &TOut,
    ) -> Result<(), ArchiveError>
    where
        TOut: IoWriterFactory<TWriter>,
        TWriter: ArchiveFileWriter,
    {
        if plan.key_len != TKey::LEN {
            return Err(ArchiveError::ContractViolation(format!(
                "a plan of keys of {} bytes; this archive's keys are {}",
                plan.key_len,
                TKey::LEN
            )));
        }

        let mut data = CompactRecordsDataFiles::new(plan);
        let mut rank = 0;

        for interval in &self.l0 {
            for (key, l2_offset) in self.source_l1_page(interval).await? {
                let bytes = key_bytes(&key)?;
                let planned = plan
                    .keys
                    .get(rank)
                    .filter(|planned| planned.key == bytes)
                    .ok_or_else(source_changed_while_compacting)?;

                let cells = self.source_l2_frame(l2_offset).await?;
                let mut cutter = CompactRecordsCutter::writing(planned.run_start);

                if let Some(mut cursor) = self.cursor(&cells) {
                    data.start_run(out, planned).await?;

                    while let Some(record) = cursor.next().await? {
                        if let Some(frame) = cutter.push(record.minute, record.bytes()) {
                            data.append(&frame).await?;
                        }
                    }
                }

                let (run, last) = cutter.finish();

                if let Some(frame) = last {
                    data.append(&frame).await?;
                }

                if run != planned.run {
                    return Err(source_changed_while_compacting());
                }

                rank += 1;
            }
        }

        if rank != plan.keys.len() {
            return Err(source_changed_while_compacting());
        }

        data.finish().await?;
        write_compact_records_index(plan, out).await
    }

    /// A walk over every record of the key `cells` belongs to, through no cache; `None` when it has none.
    fn cursor<'a>(
        &'a self,
        cells: &'a MinuteCells,
    ) -> Option<RecordsCursor<'a, TKey, TFactory, TReader>> {
        let (minute, first) = cells.first_from(0)?;
        Some(RecordsCursor::new(self, cells, minute, first, false))
    }
}

fn key_bytes<TKey: PrimaryKeyFixedSize>(key: &TKey) -> Result<Vec<u8>, ArchiveError> {
    let mut bytes = Vec::with_capacity(TKey::LEN);
    key.copy_key_checked_to(&mut bytes)?;
    Ok(bytes)
}
