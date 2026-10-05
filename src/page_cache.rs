use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rust_extensions::date_time::{AtomicDateTimeAsMicroseconds, DateTimeAsMicroseconds};
use tokio::sync::RwLock;

pub(crate) struct CachedPage<T> {
    pub value: Arc<T>,
    pub last_access: AtomicDateTimeAsMicroseconds,
}

/// Pages by offset, bounded by count.
///
/// A hit only stamps `last_access` under the READ lock, so readers of cached pages never wait on
/// each other. The WRITE lock is taken only to insert, and never across a read of the source.
pub(crate) struct PageCache<T> {
    capacity: AtomicUsize,
    pub(crate) pages: RwLock<HashMap<u64, CachedPage<T>>>,
}

impl<T> PageCache<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: AtomicUsize::new(capacity),
            pages: RwLock::new(HashMap::new()),
        }
    }

    /// Takes effect at the next insert, which evicts down to it.
    pub fn set_capacity(&self, capacity: usize) {
        self.capacity.store(capacity, Ordering::Relaxed);
    }

    pub async fn get(&self, offset: u64) -> Option<Arc<T>> {
        let pages = self.pages.read().await;
        let page = pages.get(&offset)?;
        page.last_access.update(DateTimeAsMicroseconds::now());
        Some(page.value.clone())
    }

    /// A page somebody else inserted meanwhile wins, and ours is dropped. A cache of capacity 0 keeps
    /// nothing and hands the value straight back.
    pub async fn insert(&self, offset: u64, value: T) -> Arc<T> {
        let capacity = self.capacity.load(Ordering::Relaxed);
        let mut pages = self.pages.write().await;

        if let Some(existing) = pages.get(&offset) {
            existing.last_access.update(DateTimeAsMicroseconds::now());
            return existing.value.clone();
        }

        let value = Arc::new(value);

        if capacity == 0 {
            return value;
        }

        // A linear scan on purpose, unlike the writers' caches: a hit here takes only the READ lock and
        // stamps an atomic, which no ordered index of uses could follow; the scan runs only on a miss,
        // under the write lock, over a cache of a few pages.
        while pages.len() >= capacity {
            let oldest = pages
                .iter()
                .min_by_key(|(_, page)| page.last_access.get_unix_microseconds())
                .map(|(offset, _)| *offset);

            match oldest {
                Some(oldest) => pages.remove(&oldest),
                None => break,
            };
        }

        pages.insert(
            offset,
            CachedPage {
                value: value.clone(),
                last_access: AtomicDateTimeAsMicroseconds::now(),
            },
        );

        value
    }

    /// For the SPLIT readers, which insert only through [`Self::insert_within_capacity`]: sets the capacity
    /// and evicts down to it at once, so 0 empties the cache. From then on the cache holds at most
    /// `capacity` pages. The SingleFile readers keep [`Self::set_capacity`] and [`Self::insert`].
    pub async fn resize(&self, capacity: usize) {
        let mut pages = self.pages.write().await;
        self.resize_locked(&mut pages, capacity);
    }

    /// [`Self::resize`], under the write lock of [`Self::pages`] its caller holds.
    pub fn resize_locked(&self, pages: &mut HashMap<u64, CachedPage<T>>, capacity: usize) {
        self.capacity.store(capacity, Ordering::Relaxed);
        evict_down_to(pages, capacity);
    }

    /// For the SPLIT readers: [`Self::insert`], with the capacity read under the write lock, so an insert
    /// that raced a [`Self::resize`] cannot leave the cache above the capacity it set. At capacity 0 the
    /// value is handed straight back and the cache is left empty.
    pub async fn insert_within_capacity(&self, offset: u64, value: T) -> Arc<T> {
        let mut pages = self.pages.write().await;
        let capacity = self.capacity.load(Ordering::Relaxed);

        if let Some(existing) = pages.get(&offset) {
            existing.last_access.update(DateTimeAsMicroseconds::now());
            return existing.value.clone();
        }

        let value = Arc::new(value);

        let Some(room) = capacity.checked_sub(1) else {
            evict_down_to(&mut pages, 0);
            return value;
        };

        evict_down_to(&mut pages, room);

        pages.insert(
            offset,
            CachedPage {
                value: value.clone(),
                last_access: AtomicDateTimeAsMicroseconds::now(),
            },
        );

        value
    }
}

/// Removes the pages with the oldest `last_access` until at most `keep` are left.
fn evict_down_to<T>(pages: &mut HashMap<u64, CachedPage<T>>, keep: usize) {
    while pages.len() > keep {
        let oldest = pages
            .iter()
            .min_by_key(|(_, page)| page.last_access.get_unix_microseconds())
            .map(|(offset, _)| *offset);

        match oldest {
            Some(oldest) => pages.remove(&oldest),
            None => break,
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::PageCache;

    async fn len(cache: &PageCache<u32>) -> usize {
        cache.pages.read().await.len()
    }

    #[tokio::test]
    async fn a_resize_evicts_at_once_and_the_cache_stays_within_it() {
        let cache = PageCache::new(10);

        for offset in 0..10 {
            cache.insert_within_capacity(offset, offset as u32).await;
        }
        assert_eq!(len(&cache).await, 10);

        cache.resize(3).await;
        assert_eq!(len(&cache).await, 3);

        for offset in 10..20 {
            cache.insert_within_capacity(offset, 0).await;
            assert!(len(&cache).await <= 3, "after inserting {offset}");
        }
        assert_eq!(len(&cache).await, 3);

        cache.resize(0).await;
        assert_eq!(len(&cache).await, 0);

        for offset in 20..30 {
            assert_eq!(*cache.insert_within_capacity(offset, 5).await, 5);
            assert!(cache.get(offset).await.is_none());
        }
        assert_eq!(len(&cache).await, 0);

        cache.resize(2).await;
        cache.insert_within_capacity(1, 1).await;
        cache.insert_within_capacity(2, 2).await;
        cache.insert_within_capacity(3, 3).await;
        assert_eq!(len(&cache).await, 2);
    }

    #[tokio::test]
    async fn a_page_already_cached_wins_over_the_one_inserted() {
        let cache = PageCache::new(2);

        let first = cache.insert_within_capacity(7, 1).await;
        let second = cache.insert_within_capacity(7, 2).await;

        assert_eq!((*first, *second), (1, 1));
        assert_eq!(len(&cache).await, 1);
    }
}
