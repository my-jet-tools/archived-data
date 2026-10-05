use crate::ArchiveError;

/// One slot's content. `Default` is the EMPTY item: a new data page is filled with it, so a slot
/// nobody wrote reads back as `Default`.
pub trait PreallocatedDataItem: Default {
    const ITEM_LEN: usize;

    /// `src` is exactly [`Self::ITEM_LEN`] bytes.
    fn from_slice(src: &[u8]) -> Self;

    /// Appends exactly [`Self::ITEM_LEN`] bytes. The crate checks it.
    fn copy_to(&self, out: &mut Vec<u8>);
}

/// The length check every item write goes through, out of reach of the implementations.
pub(crate) trait CheckedItemBytes {
    fn copy_item_checked_to(&self, out: &mut Vec<u8>) -> Result<(), ArchiveError>;
}

impl<TItem: PreallocatedDataItem> CheckedItemBytes for TItem {
    fn copy_item_checked_to(&self, out: &mut Vec<u8>) -> Result<(), ArchiveError> {
        let before = out.len();
        self.copy_to(out);
        let written = out.len() - before;

        if written != TItem::ITEM_LEN {
            return Err(ArchiveError::ContractViolation(format!(
                "PreallocatedDataItem::copy_to wrote {written} bytes, ITEM_LEN is {}",
                TItem::ITEM_LEN
            )));
        }

        Ok(())
    }
}
