use crate::{
    AppendFrontier, ArchiveError, ArchiveFileWriter, IoWriterFactory,
    MinuteIndexedRecordsCompactionPlan, PlannedRecordsKey, source_changed_while_compacting,
};

/// The compact data files of a plan, appended run by run: a file is created when the first run landing in
/// it starts, and finished — at exactly its planned size — before the next is created.
pub(crate) struct CompactRecordsDataFiles<'a, TWriter> {
    plan: &'a MinuteIndexedRecordsCompactionPlan,
    current: Option<(u8, TWriter, AppendFrontier)>,
    created: usize,
}

impl<'a, TWriter: ArchiveFileWriter> CompactRecordsDataFiles<'a, TWriter> {
    pub fn new(plan: &'a MinuteIndexedRecordsCompactionPlan) -> Self {
        Self {
            plan,
            current: None,
            created: 0,
        }
    }

    /// Makes the file `planned` lands in the current one, which must end exactly where its run starts.
    pub async fn start_run<TOut: IoWriterFactory<TWriter>>(
        &mut self,
        out: &TOut,
        planned: &PlannedRecordsKey,
    ) -> Result<(), ArchiveError> {
        let current_no = self.current.as_ref().map(|(no, _, _)| *no);

        if current_no != Some(planned.file_no) {
            if planned.file_no as usize != self.created {
                return Err(source_changed_while_compacting());
            }

            self.close().await?;
            let writer = out.create_data_writer(planned.file_no).await?;
            self.current = Some((planned.file_no, writer, AppendFrontier::new(0)));
            self.created += 1;
        }

        let (_, _, frontier) = self.current.as_ref().unwrap();

        if frontier.appended() != planned.run_start as u64 {
            return Err(source_changed_while_compacting());
        }

        Ok(())
    }

    pub async fn append(&mut self, frame: &[u8]) -> Result<(), ArchiveError> {
        let Some((no, writer, frontier)) = self.current.as_mut() else {
            return Err(source_changed_while_compacting());
        };

        let offset = frontier.reserve(frame.len());
        let planned = self.plan.data_file_sizes.get(*no as usize).copied();

        if planned.is_none_or(|size| frontier.reserved() > size) {
            return Err(source_changed_while_compacting());
        }

        frontier.check_append(offset, frame.len())?;
        let returned = writer.append(frame).await?;
        frontier.confirm_append(returned, frame.len())
    }

    /// Checks every planned file came, the last one at its planned size, and finishes it.
    pub async fn finish(mut self) -> Result<(), ArchiveError> {
        self.close().await?;

        if self.created != self.plan.data_file_sizes.len() {
            return Err(source_changed_while_compacting());
        }

        Ok(())
    }

    async fn close(&mut self) -> Result<(), ArchiveError> {
        let Some((no, writer, frontier)) = self.current.take() else {
            return Ok(());
        };

        if self.plan.data_file_sizes.get(no as usize) != Some(&frontier.appended()) {
            return Err(source_changed_while_compacting());
        }

        Ok(writer.finish().await?)
    }
}
