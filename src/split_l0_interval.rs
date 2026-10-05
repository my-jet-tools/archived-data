use crate::ArchiveError;

/// One L1 page of the split index, named by the lowest and the highest key on it. What a key's bytes are
/// is the module's; the interval itself is the same in every module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitL0Interval<TKey> {
    pub from: TKey,
    pub to: TKey,
    pub l1_offset: u32,
}

impl<TKey: Ord> SplitL0Interval<TKey> {
    /// Refuses an L1 page whose first key is not `from` or whose last key is not `to` — an empty page
    /// included. A page that does not span its interval exactly would make a binary search answer "absent"
    /// for a key that is there, and a range walk repeat or skip keys, so it is `Corrupted` whether it was
    /// just decoded or found in a cache.
    pub fn check_spanned_by(
        &self,
        first: Option<&TKey>,
        last: Option<&TKey>,
    ) -> Result<(), ArchiveError> {
        let spans = first.is_some_and(|first| *first == self.from)
            && last.is_some_and(|last| *last == self.to);

        if !spans {
            return Err(ArchiveError::Corrupted(format!(
                "the L1 page at {} does not span its L0 interval",
                self.l1_offset
            )));
        }

        Ok(())
    }
}

/// The interval of `l0` — ordered and disjoint — whose bounds hold `key`, if any.
pub(crate) fn split_l0_interval_of<'a, TKey: Ord>(
    l0: &'a [SplitL0Interval<TKey>],
    key: &TKey,
) -> Option<&'a SplitL0Interval<TKey>> {
    let index = l0.partition_point(|interval| interval.to < *key);
    l0.get(index).filter(|interval| interval.from <= *key)
}

#[cfg(test)]
mod tests {
    use crate::{ArchiveError, SplitL0Interval, split_l0_interval_of};

    fn interval(from: u32, to: u32, l1_offset: u32) -> SplitL0Interval<u32> {
        SplitL0Interval {
            from,
            to,
            l1_offset,
        }
    }

    #[test]
    fn a_key_is_found_only_inside_the_bounds_of_an_interval() {
        let l0 = [
            interval(10, 20, 100),
            interval(30, 30, 200),
            interval(40, 50, 300),
        ];

        let found = |key| split_l0_interval_of(&l0, &key).map(|interval| interval.l1_offset);

        for (key, expected) in [
            (9, None),
            (10, Some(100)),
            (15, Some(100)),
            (20, Some(100)),
            (25, None),
            (30, Some(200)),
            (45, Some(300)),
            (50, Some(300)),
            (51, None),
        ] {
            assert_eq!(found(key), expected, "key {key}");
        }

        assert_eq!(split_l0_interval_of(&[], &1u32), None);
    }

    #[test]
    fn a_page_spans_its_interval_only_with_exactly_its_bounds() {
        let bounds = interval(10, 20, 7);

        bounds.check_spanned_by(Some(&10), Some(&20)).unwrap();

        for (first, last) in [
            (Some(&11), Some(&20)),
            (Some(&10), Some(&19)),
            (Some(&10), None),
            (None, None),
        ] {
            match bounds.check_spanned_by(first, last) {
                Err(ArchiveError::Corrupted(message)) => {
                    assert!(message.contains("L1 page at 7"), "{message}")
                }
                other => panic!("{first:?}..{last:?}: {other:?}"),
            }
        }
    }
}
