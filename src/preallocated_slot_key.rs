use crate::ArchiveError;

/// Which preallocated slot of a data page an item lives in.
pub trait PreallocatedSlotKey {
    const SLOTS: usize;

    /// Below [`Self::SLOTS`]. The crate checks it.
    fn slot_index(&self) -> usize;
}

/// The check every slot index goes through. A trait of the crate's own with one blanket
/// implementation, so no implementation of [`PreallocatedSlotKey`] can override it away.
pub(crate) trait CheckedSlotIndex {
    fn checked_slot_index(&self) -> Result<usize, ArchiveError>;
}

impl<TSlot: PreallocatedSlotKey> CheckedSlotIndex for TSlot {
    fn checked_slot_index(&self) -> Result<usize, ArchiveError> {
        let index = self.slot_index();

        if index >= TSlot::SLOTS {
            return Err(ArchiveError::ContractViolation(format!(
                "slot index {index} is not below SLOTS {}",
                TSlot::SLOTS
            )));
        }

        Ok(index)
    }
}
