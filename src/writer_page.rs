use std::collections::HashMap;

/// A page a writer holds in memory: `dirty` until it reaches the sink, `last_used` for eviction.
pub(crate) struct WriterPage<T> {
    pub value: T,
    pub dirty: bool,
    pub last_used: u64,
}

pub(crate) fn least_recently_used<T>(pages: &HashMap<u64, WriterPage<T>>) -> Option<u64> {
    pages
        .iter()
        .min_by_key(|(_, page)| page.last_used)
        .map(|(offset, _)| *offset)
}

/// Ascending, so a flush writes the file front to back.
pub(crate) fn dirty_offsets<T>(pages: &HashMap<u64, WriterPage<T>>) -> Vec<u64> {
    let mut offsets: Vec<u64> = pages
        .iter()
        .filter(|(_, page)| page.dirty)
        .map(|(offset, _)| *offset)
        .collect();
    offsets.sort_unstable();
    offsets
}
