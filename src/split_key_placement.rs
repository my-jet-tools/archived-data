/// Where a key that is not in a split archive yet goes, decided by the writer before it changes anything.
/// An interval is a position in L0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitKeyPlacement {
    /// Onto a new L1 page of its own, whose interval takes L0 position `at`.
    NewPage { at: usize },
    /// Into the page of `interval`, which has room for it.
    Insert { interval: usize },
    /// Into the full page of `interval`, whose split-off upper part then moves in front of the next page's
    /// keys.
    SplitIntoNext { interval: usize },
    /// Into the full page of `interval`, whose split-off upper part then goes onto a new page right after
    /// it.
    SplitIntoNewPage { interval: usize },
}
