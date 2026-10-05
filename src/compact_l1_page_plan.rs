/// One L1 page of a compact index: its first and last key as the index stores them, how many keys it
/// holds and its exact length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactL1PagePlan {
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub count: usize,
    pub len: usize,
}
