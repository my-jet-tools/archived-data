use crate::{ArchiveError, CompactL1PagePlan, DataFileLayout, MAX_INDEX_BYTES};

/// What a compaction will write, worked out before a byte of it is written: the compact L1 pages, and
/// the exact size of the compact index and of every compact data file.
///
/// A caller whose writers must know a file's size up front (an S3 upload) builds its factory from
/// [`Self::index_size`] and [`Self::data_file_sizes`], then hands the plan back to `write_compacted_to`,
/// which refuses a source that no longer matches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    pub(crate) pages: Vec<CompactL1PagePlan>,
    pub(crate) keys: usize,
    pub(crate) l0_len: u64,
    pub(crate) index_size: u64,
    pub(crate) layout: DataFileLayout,
    pub(crate) data_file_sizes: Vec<u64>,
}

impl CompactionPlan {
    /// `l0_len` is the exact length of the one L0 frame. An index past [`MAX_INDEX_BYTES`], or data needing
    /// data file 256, is a `ContractViolation`.
    pub(crate) fn new(
        pages: Vec<CompactL1PagePlan>,
        l0_len: u64,
        layout: DataFileLayout,
    ) -> Result<Self, ArchiveError> {
        let keys = pages.iter().map(|page| page.count).sum();

        let index_size = pages
            .iter()
            .try_fold(l0_len, |size, page| size.checked_add(page.len as u64))
            .filter(|size| *size <= MAX_INDEX_BYTES);

        let Some(index_size) = index_size else {
            return Err(ArchiveError::ContractViolation(format!(
                "a compact index of {} L1 pages would exceed {MAX_INDEX_BYTES} bytes",
                pages.len()
            )));
        };

        let data_file_sizes = layout.file_sizes(keys)?;

        Ok(Self {
            pages,
            keys,
            l0_len,
            index_size,
            layout,
            data_file_sizes,
        })
    }

    pub fn index_size(&self) -> u64 {
        self.index_size
    }

    /// One size per data file, in file order; none for an empty archive.
    pub fn data_file_sizes(&self) -> &[u64] {
        &self.data_file_sizes
    }

    pub fn keys_count(&self) -> usize {
        self.keys
    }

    /// Where each compact L1 page starts: right after the L0 frame, one after another.
    pub(crate) fn l1_offsets(&self) -> Vec<u32> {
        let mut offset = self.l0_len;

        self.pages
            .iter()
            .map(|page| {
                // Below `index_size`, which is at most 4 GiB.
                let at = offset as u32;
                offset += page.len as u64;
                at
            })
            .collect()
    }
}

pub(crate) fn source_changed_while_compacting() -> ArchiveError {
    ArchiveError::Corrupted("the source changed while it was being compacted".to_string())
}

/// A compaction walks the keys of every L1 page in L0 order, and each must be above the one before it.
pub(crate) fn refuse_unordered<TKey: Ord>(
    previous: &Option<TKey>,
    key: &TKey,
) -> Result<(), ArchiveError> {
    if previous.as_ref().is_some_and(|previous| previous >= key) {
        return Err(ArchiveError::Corrupted(
            "keys do not ascend across L1 pages".to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(count: usize, len: usize) -> CompactL1PagePlan {
        CompactL1PagePlan {
            from: vec![1],
            to: vec![2],
            count,
            len,
        }
    }

    #[test]
    fn an_empty_archive_plans_its_l0_frame_and_no_data_files() {
        let plan = CompactionPlan::new(Vec::new(), 20, DataFileLayout::new(22, 44)).unwrap();

        assert_eq!(plan.index_size(), 20);
        assert_eq!(plan.data_file_sizes(), &[] as &[u64]);
        assert_eq!(plan.keys_count(), 0);
        assert!(plan.l1_offsets().is_empty());
    }

    #[test]
    fn the_sizes_are_the_l0_frame_the_pages_and_the_layout() {
        let plan = CompactionPlan::new(
            vec![page(3, 100), page(2, 70)],
            40,
            DataFileLayout::new(22, 44),
        )
        .unwrap();

        assert_eq!(plan.index_size(), 210);
        assert_eq!(plan.keys_count(), 5);
        assert_eq!(plan.l1_offsets(), [40, 140]);
        assert_eq!(plan.data_file_sizes(), [44, 44, 22]);
    }

    #[test]
    fn an_index_past_four_gib_or_a_257th_data_file_is_a_contract_violation() {
        let past_4_gib = CompactionPlan::new(
            vec![page(1, (MAX_INDEX_BYTES - 19) as usize)],
            20,
            DataFileLayout::new(22, 22),
        );
        assert!(matches!(
            past_4_gib,
            Err(ArchiveError::ContractViolation(_))
        ));

        let exactly_4_gib = CompactionPlan::new(
            vec![page(1, (MAX_INDEX_BYTES - 20) as usize)],
            20,
            DataFileLayout::new(22, 22),
        );
        assert_eq!(exactly_4_gib.unwrap().index_size(), MAX_INDEX_BYTES);

        let files_257 = CompactionPlan::new(vec![page(257, 100)], 20, DataFileLayout::new(22, 22));
        assert!(matches!(files_257, Err(ArchiveError::ContractViolation(_))));

        assert!(matches!(
            source_changed_while_compacting(),
            ArchiveError::Corrupted(_)
        ));
    }

    #[test]
    fn a_key_not_above_the_previous_one_is_corrupted() {
        refuse_unordered(&None, &5).unwrap();
        refuse_unordered(&Some(4), &5).unwrap();

        for previous in [5, 6] {
            assert!(matches!(
                refuse_unordered(&Some(previous), &5),
                Err(ArchiveError::Corrupted(_))
            ));
        }
    }
}
