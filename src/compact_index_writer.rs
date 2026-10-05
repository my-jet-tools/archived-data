use crate::{
    AppendFrontier, ArchiveError, ArchiveFileWriter, CompactionPlan, IoWriterFactory,
    source_changed_while_compacting,
};

/// Writes the compact index of a [`CompactionPlan`], append only: the one L0 frame, then every L1 page in
/// key order, each checked against the page the plan worked out. What the entries look like is the
/// module's. A page other than planned, or an index that ends at a size other than planned, means the
/// source changed: `Corrupted`.
pub(crate) struct CompactIndexWriter<'a, TWriter> {
    plan: &'a CompactionPlan,
    writer: TWriter,
    frontier: AppendFrontier,
    pages: usize,
}

impl<'a, TWriter: ArchiveFileWriter> CompactIndexWriter<'a, TWriter> {
    /// Creates the index and appends `l0_frame`. The frame is built from the plan itself, so a length
    /// other than the planned one is a plan of another key type: a `ContractViolation`.
    pub async fn create<TFactory: IoWriterFactory<TWriter>>(
        factory: &TFactory,
        plan: &'a CompactionPlan,
        l0_frame: &[u8],
    ) -> Result<Self, ArchiveError> {
        if l0_frame.len() as u64 != plan.l0_len {
            return Err(ArchiveError::ContractViolation(format!(
                "a compact L0 frame of {} bytes; the plan's is {}",
                l0_frame.len(),
                plan.l0_len
            )));
        }

        let mut index = Self {
            plan,
            writer: factory.create_index_writer().await?,
            frontier: AppendFrontier::new(0),
            pages: 0,
        };

        index.append(l0_frame).await?;
        Ok(index)
    }

    /// The next L1 page, holding `count` keys from `from` to `to` as the index stores them.
    pub async fn push_l1_page(
        &mut self,
        page: &[u8],
        count: usize,
        from: &[u8],
        to: &[u8],
    ) -> Result<(), ArchiveError> {
        let planned = self.plan.pages.get(self.pages).is_some_and(|planned| {
            planned.count == count
                && planned.len == page.len()
                && planned.from == from
                && planned.to == to
        });

        if !planned {
            return Err(source_changed_while_compacting());
        }

        self.append(page).await?;
        self.pages += 1;
        Ok(())
    }

    /// Checks every planned page came and the index is exactly its planned size, then finishes it.
    pub async fn finish(self) -> Result<(), ArchiveError> {
        if self.pages != self.plan.pages.len() || self.frontier.appended() != self.plan.index_size {
            return Err(source_changed_while_compacting());
        }

        Ok(self.writer.finish().await?)
    }

    async fn append(&mut self, bytes: &[u8]) -> Result<(), ArchiveError> {
        let offset = self.frontier.reserve(bytes.len());
        self.frontier.check_append(offset, bytes.len())?;
        let returned = self.writer.append(bytes).await?;
        self.frontier.confirm_append(returned, bytes.len())
    }
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryWriter};
    use crate::{
        ArchiveError, CompactIndexWriter, CompactL1PagePlan, CompactionPlan, DataFileLayout,
        l0_frame, l1_frame,
    };

    type Index<'a> = CompactIndexWriter<'a, InMemoryWriter>;

    /// Two pages of 20 bytes after a 24-byte L0 frame.
    fn plan() -> CompactionPlan {
        let pages = vec![
            CompactL1PagePlan {
                from: vec![1],
                to: vec![2],
                count: 2,
                len: 20,
            },
            CompactL1PagePlan {
                from: vec![3],
                to: vec![3],
                count: 1,
                len: 20,
            },
        ];

        CompactionPlan::new(pages, 24, DataFileLayout::new(22, 44)).unwrap()
    }

    fn is_corrupted<T>(result: &Result<T, ArchiveError>) -> bool {
        matches!(result, Err(ArchiveError::Corrupted(_)))
    }

    #[tokio::test]
    async fn the_index_is_the_l0_frame_then_the_planned_pages_appended_and_finished() {
        let io = InMemoryIo::default();
        let plan = plan();
        let l0 = l0_frame(24, 0, 2, &[9; 4]);

        let mut index = Index::create(&io, &plan, &l0).await.unwrap();
        index
            .push_l1_page(&l1_frame(20, 0, 2, &[1]), 2, &[1], &[2])
            .await
            .unwrap();
        index
            .push_l1_page(&l1_frame(20, 0, 1, &[3]), 1, &[3], &[3])
            .await
            .unwrap();
        index.finish().await.unwrap();

        let file = io.file(FileRole::Index).unwrap();
        assert_eq!(file.bytes().await.len() as u64, plan.index_size());
        assert_eq!(
            (file.appends(), file.write_ats(), file.finishes()),
            (3, 0, 1)
        );
    }

    #[tokio::test]
    async fn a_page_other_than_planned_or_a_missing_page_is_corrupted() {
        let io = InMemoryIo::default();
        let plan = plan();
        let l0 = l0_frame(24, 0, 2, &[]);
        let page = l1_frame(20, 0, 2, &[]);

        for (count, len, from, to) in [(3, 20, 1, 2), (2, 21, 1, 2), (2, 20, 0, 2), (2, 20, 1, 4)] {
            let mut index = Index::create(&io, &plan, &l0).await.unwrap();
            let mut bytes = page.clone();
            bytes.resize(len, 0);

            assert!(is_corrupted(
                &index.push_l1_page(&bytes, count, &[from], &[to]).await
            ));
        }

        let mut short = Index::create(&io, &plan, &l0).await.unwrap();
        short.push_l1_page(&page, 2, &[1], &[2]).await.unwrap();
        assert!(is_corrupted(&short.finish().await));

        let mut long = Index::create(&io, &plan, &l0).await.unwrap();
        long.push_l1_page(&page, 2, &[1], &[2]).await.unwrap();
        long.push_l1_page(&page, 1, &[3], &[3]).await.unwrap();
        assert!(is_corrupted(&long.push_l1_page(&page, 1, &[4], &[4]).await));
    }

    #[tokio::test]
    async fn an_l0_frame_of_another_length_is_a_contract_violation_before_the_index_exists() {
        let io = InMemoryIo::default();
        let plan = plan();

        assert!(matches!(
            Index::create(&io, &plan, &l0_frame(25, 0, 2, &[])).await,
            Err(ArchiveError::ContractViolation(_))
        ));
        assert!(io.created_roles().is_empty());
    }
}
