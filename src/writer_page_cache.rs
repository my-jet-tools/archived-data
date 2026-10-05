use std::collections::{BTreeMap, HashMap};

use crate::WriterPage;

/// The pages a split writer holds in memory, by offset, with their least recently used one found in
/// `O(log n)`: beside the map, every page is filed under the tick it was last used at, and a use moves it
/// to a new tick. Ticks are never reused, so the order is exactly the order of the uses.
///
/// The cache holds pages and nothing else: when to evict, and where an evicted page's bytes go, are the
/// owner's.
pub(crate) struct WriterPageCache<T> {
    pages: HashMap<u64, WriterPage<T>>,
    by_use: BTreeMap<u64, u64>,
    tick: u64,
}

impl<T> WriterPageCache<T> {
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            by_use: BTreeMap::new(),
            tick: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// A cached page, NOT stamped as used.
    pub fn get(&self, offset: u64) -> Option<&T> {
        self.pages.get(&offset).map(|page| &page.value)
    }

    pub fn is_dirty(&self, offset: u64) -> bool {
        self.pages.get(&offset).is_some_and(|page| page.dirty)
    }

    /// Stamps a cached page as the most recently used; `false` when it is not cached.
    pub fn touch(&mut self, offset: u64) -> bool {
        let tick = self.next_tick();

        let Some(page) = self.pages.get_mut(&offset) else {
            return false;
        };

        self.by_use.remove(&page.last_used);
        page.last_used = tick;
        self.by_use.insert(tick, offset);
        true
    }

    /// A cached page to change: stamped as the most recently used and marked dirty.
    pub fn modify(&mut self, offset: u64) -> Option<&mut T> {
        if !self.touch(offset) {
            return None;
        }

        let page = self.pages.get_mut(&offset)?;
        page.dirty = true;
        Some(&mut page.value)
    }

    /// Caches `value` at `offset` as the most recently used page, replacing a page cached there. Nothing is
    /// evicted.
    pub fn insert(&mut self, offset: u64, value: T, dirty: bool) {
        let tick = self.next_tick();

        let replaced = self.pages.insert(
            offset,
            WriterPage {
                value,
                dirty,
                last_used: tick,
            },
        );

        if let Some(replaced) = replaced {
            self.by_use.remove(&replaced.last_used);
        }

        self.by_use.insert(tick, offset);
    }

    /// The page's bytes reached its file: it is no longer dirty, and its use is not stamped.
    pub fn mark_clean(&mut self, offset: u64) {
        if let Some(page) = self.pages.get_mut(&offset) {
            page.dirty = false;
        }
    }

    pub fn remove(&mut self, offset: u64) -> Option<T> {
        let page = self.pages.remove(&offset)?;
        self.by_use.remove(&page.last_used);
        Some(page.value)
    }

    pub fn least_recently_used(&self) -> Option<u64> {
        self.by_use.first_key_value().map(|(_, offset)| *offset)
    }

    /// Ascending, so a flush writes the file front to back.
    pub fn dirty_offsets(&self) -> Vec<u64> {
        let mut offsets: Vec<u64> = self
            .pages
            .iter()
            .filter(|(_, page)| page.dirty)
            .map(|(offset, _)| *offset)
            .collect();
        offsets.sort_unstable();
        offsets
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::least_recently_used;

    type Cache = WriterPageCache<&'static str>;

    #[test]
    fn the_victim_is_the_page_used_longest_ago() {
        let mut cache = Cache::new();
        assert_eq!(cache.least_recently_used(), None);

        cache.insert(30, "c", false);
        cache.insert(10, "a", true);
        cache.insert(20, "b", false);
        assert_eq!(cache.least_recently_used(), Some(30));

        assert!(cache.touch(30));
        assert_eq!(cache.least_recently_used(), Some(10));

        *cache.modify(10).unwrap() = "A";
        assert_eq!(cache.least_recently_used(), Some(20));
        assert!(!cache.touch(99));
        assert!(cache.modify(99).is_none());

        assert_eq!(cache.remove(20), Some("b"));
        assert_eq!(cache.least_recently_used(), Some(30));
        assert_eq!(cache.get(10), Some(&"A"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn dirt_is_set_by_a_change_and_cleared_only_by_a_store() {
        let mut cache = Cache::new();
        cache.insert(40, "d", false);
        cache.insert(8, "e", true);
        cache.insert(16, "f", false);

        assert_eq!(cache.dirty_offsets(), [8]);
        cache.modify(40);
        assert_eq!(cache.dirty_offsets(), [8, 40]);
        assert!(cache.is_dirty(40) && !cache.is_dirty(16) && !cache.is_dirty(99));

        let before = cache.least_recently_used();
        cache.mark_clean(8);
        assert_eq!(cache.dirty_offsets(), [40]);
        assert_eq!(cache.least_recently_used(), before, "a store is not a use");
    }

    #[test]
    fn a_replaced_page_leaves_no_stale_use_behind() {
        let mut cache = Cache::new();
        cache.insert(1, "old", true);
        cache.insert(2, "other", false);
        cache.insert(1, "new", false);

        assert_eq!(cache.least_recently_used(), Some(2));
        assert_eq!(cache.remove(2), Some("other"));
        assert_eq!(cache.least_recently_used(), Some(1));
        assert_eq!(cache.remove(1), Some("new"));
        assert_eq!(cache.least_recently_used(), None);
        assert!(cache.get(1).is_none());
    }

    /// The same uses, replayed on the linear scan the SingleFile writers keep, pick the same victims.
    #[test]
    fn it_evicts_exactly_what_the_linear_scan_evicts() {
        let mut cache = WriterPageCache::new();
        let mut scanned: HashMap<u64, WriterPage<u64>> = HashMap::new();
        let mut tick = 0;
        let mut state = 0x2545_F491_4F6C_DD1Du64;

        for _ in 0..5_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let offset = state % 64;
            tick += 1;

            match (state >> 8) % 3 {
                0 if cache.get(offset).is_some() => {
                    cache.touch(offset);
                    scanned.get_mut(&offset).unwrap().last_used = tick;
                }
                1 => {
                    cache.insert(offset, offset, false);
                    scanned.insert(
                        offset,
                        WriterPage {
                            value: offset,
                            dirty: false,
                            last_used: tick,
                        },
                    );
                }
                _ => {
                    let victim = cache.least_recently_used();
                    assert_eq!(victim, least_recently_used(&scanned));

                    if let Some(victim) = victim {
                        cache.remove(victim);
                        scanned.remove(&victim);
                    }
                }
            }

            assert_eq!(cache.len(), scanned.len());
        }
    }
}
