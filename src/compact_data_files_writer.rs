use std::marker::PhantomData;

use crate::{
    AppendFrontier, ArchiveError, ArchiveFileWriter, CompactionPlan, IoWriterFactory,
    PreallocatedDataItem, PreallocatedSlotKey, encode_split_data_page,
    source_changed_while_compacting,
};

/// Writes the compact data files of a [`CompactionPlan`]: one page per key in ascending key order, each
/// as ONE frame, appended only. A data file is created when its first page comes and finished before the
/// next one is created. A source that yields more or fewer pages than planned, or a file that ends at a
/// size other than planned, is `Corrupted`.
pub(crate) struct CompactDataFilesWriter<'a, TSlotKey, TItem, TWriter> {
    plan: &'a CompactionPlan,
    current: Option<(u8, TWriter, AppendFrontier)>,
    pages: usize,
    _types: PhantomData<fn() -> (TSlotKey, TItem)>,
}

impl<'a, TSlotKey, TItem, TWriter> CompactDataFilesWriter<'a, TSlotKey, TItem, TWriter>
where
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TWriter: ArchiveFileWriter,
{
    pub fn new(plan: &'a CompactionPlan) -> Self {
        Self {
            plan,
            current: None,
            pages: 0,
            _types: PhantomData,
        }
    }

    /// The next page, from the `SLOTS × ITEM_LEN` body of the next key's data page.
    pub async fn push<TFactory: IoWriterFactory<TWriter>>(
        &mut self,
        factory: &TFactory,
        body: &[u8],
    ) -> Result<(), ArchiveError> {
        if self.pages >= self.plan.keys {
            return Err(source_changed_while_compacting());
        }

        let page = encode_split_data_page::<TSlotKey, TItem>(body)?;

        if page.len() != self.plan.layout.page_len() {
            return Err(ArchiveError::ContractViolation(format!(
                "a data page of {} bytes; the plan's pages are {}",
                page.len(),
                self.plan.layout.page_len()
            )));
        }

        let at = self.plan.layout.ref_of(self.pages)?;

        if self
            .current
            .as_ref()
            .is_none_or(|(no, _, _)| *no != at.file_no)
        {
            if let Some((no, writer, frontier)) = self.current.take() {
                close(self.plan, no, writer, frontier).await?;
            }

            let writer = factory.create_data_writer(at.file_no).await?;
            self.current = Some((at.file_no, writer, AppendFrontier::new(0)));
        }

        let (_, writer, frontier) = self.current.as_mut().unwrap();
        frontier.reserve(page.len());
        frontier.check_append(at.offset as u64, page.len())?;
        let returned = writer.append(&page).await?;
        frontier.confirm_append(returned, page.len())?;

        self.pages += 1;
        Ok(())
    }

    /// Checks every planned page came, then finishes the last data file. With no keys nothing was created.
    pub async fn finish(mut self) -> Result<(), ArchiveError> {
        if self.pages != self.plan.keys {
            return Err(source_changed_while_compacting());
        }

        match self.current.take() {
            Some((no, writer, frontier)) => close(self.plan, no, writer, frontier).await,
            None => Ok(()),
        }
    }
}

async fn close<TWriter: ArchiveFileWriter>(
    plan: &CompactionPlan,
    no: u8,
    writer: TWriter,
    frontier: AppendFrontier,
) -> Result<(), ArchiveError> {
    if plan.data_file_sizes.get(no as usize) != Some(&frontier.appended()) {
        return Err(source_changed_while_compacting());
    }

    Ok(writer.finish().await?)
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryWriter};
    use crate::{
        ArchiveError, CompactDataFilesWriter, CompactL1PagePlan, CompactionPlan, DataFileLayout,
        TinyItem, TinySlot, data_frame,
    };

    type Data<'a> = CompactDataFilesWriter<'a, TinySlot, TinyItem, InMemoryWriter>;

    fn plan(keys: usize) -> CompactionPlan {
        let pages = if keys == 0 {
            Vec::new()
        } else {
            vec![CompactL1PagePlan {
                from: vec![0],
                to: vec![keys as u8],
                count: keys,
                len: 100,
            }]
        };

        CompactionPlan::new(pages, 24, DataFileLayout::new(22, 44)).unwrap()
    }

    #[tokio::test]
    async fn pages_fill_the_planned_files_in_order_each_finished_before_the_next() {
        let io = InMemoryIo::default();
        let plan = plan(5);
        let mut data = Data::new(&plan);

        for key in 0..5u8 {
            data.push(&io, &[key; 12]).await.unwrap();

            if key == 2 {
                let data_0 = io.file(FileRole::Data(0)).unwrap();
                assert_eq!(data_0.finishes(), 1);
            }
        }
        data.finish().await.unwrap();

        assert_eq!(
            io.created_roles(),
            [FileRole::Data(0), FileRole::Data(1), FileRole::Data(2)]
        );

        let expected = [
            [
                data_frame(22, 0, 4, &[0; 12]),
                data_frame(22, 0, 4, &[1; 12]),
            ]
            .concat(),
            [
                data_frame(22, 0, 4, &[2; 12]),
                data_frame(22, 0, 4, &[3; 12]),
            ]
            .concat(),
            data_frame(22, 0, 4, &[4; 12]),
        ];

        for (no, bytes) in expected.into_iter().enumerate() {
            let file = io.file(FileRole::Data(no as u8)).unwrap();
            assert_eq!(file.bytes().await.len() as u64, plan.data_file_sizes()[no]);
            assert_eq!(file.bytes().await, bytes);
            assert_eq!((file.write_ats(), file.finishes()), (0, 1));
        }
    }

    #[tokio::test]
    async fn no_keys_creates_no_data_file() {
        let io = InMemoryIo::default();
        let plan = plan(0);

        Data::new(&plan).finish().await.unwrap();
        assert!(io.created_roles().is_empty());
    }

    #[tokio::test]
    async fn more_or_fewer_pages_than_planned_is_corrupted() {
        let io = InMemoryIo::default();
        let plan = plan(3);

        let mut more = Data::new(&plan);
        for key in 0..3u8 {
            more.push(&io, &[key; 12]).await.unwrap();
        }
        assert!(matches!(
            more.push(&io, &[9; 12]).await,
            Err(ArchiveError::Corrupted(_))
        ));

        let mut fewer = Data::new(&plan);
        fewer.push(&io, &[0; 12]).await.unwrap();
        assert!(matches!(
            fewer.finish().await,
            Err(ArchiveError::Corrupted(_))
        ));
    }

    #[tokio::test]
    async fn a_body_of_the_wrong_length_is_a_contract_violation() {
        let io = InMemoryIo::default();
        let plan = plan(1);

        assert!(matches!(
            Data::new(&plan).push(&io, &[0; 11]).await,
            Err(ArchiveError::ContractViolation(_))
        ));
        assert!(io.created_roles().is_empty());
    }
}
