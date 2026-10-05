//! How a split reader walks from L0 to the L1 page of an interval and on to a data page, through its
//! caches. Nothing here knows how a page's bytes decode: a module hands in the load of the page it reads,
//! and gets back the same walk in every module.

use std::future::Future;
use std::sync::Arc;

use crate::{
    ArchiveError, ArchiveFileReader, CheckedSlotIndex, DataRef, IoReaderFactory, LazyDataReaders,
    PageCache, PreallocatedDataItem, PreallocatedSlotKey, SplitL0Interval, items_of_split_body,
    read_split_data_page, split_l0_interval_of,
};

/// The entries of an L1 page as a reader caches them: every key with its data page, ascending.
pub(crate) type SplitL1Page<TKey> = Vec<(TKey, DataRef)>;

/// What a split reader's `get` answers: the items of slots `from..=to` of the key `data_ref` finds, in
/// slot order, out of the data page `body_of` hands over. `from > to` is `Some(vec![])` at once, WITHOUT
/// reading anything; a key `data_ref` does not find is `None`; a slot index out of range is refused first.
pub(crate) async fn split_get_items<TSlot, TItem, TRef, TBody>(
    from: TSlot,
    to: TSlot,
    data_ref: impl FnOnce() -> TRef,
    body_of: impl FnOnce(DataRef) -> TBody,
) -> Result<Option<Vec<TItem>>, ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TRef: Future<Output = Result<Option<DataRef>, ArchiveError>>,
    TBody: Future<Output = Result<Arc<Vec<u8>>, ArchiveError>>,
{
    let from = from.checked_slot_index()?;
    let to = to.checked_slot_index()?;

    if from > to {
        return Ok(Some(Vec::new()));
    }

    let Some(at) = data_ref().await? else {
        return Ok(None);
    };

    let body = body_of(at).await?;
    Ok(Some(items_of_split_body::<TItem>(&body, from, to)))
}

/// The L1 page of `interval` through `cache`: a hit, or on a miss the page `load` reads, inserted.
///
/// Whichever page comes back — a hit, the page just loaded, or one another caller inserted meanwhile — must
/// span `interval` exactly, checked in memory, or it is `Corrupted`. A load checks that already; a cached
/// page was checked against the interval that loaded it, which in a damaged index — two intervals naming
/// one page — is not this one, and must never answer `Ok` with keys repeated, out of order or missing.
///
/// What an entry holds besides its key is the module's: a data page in the preallocated modules, an L2
/// frame in the minute-indexed one, a login's year blocks in the global day index.
pub(crate) async fn split_l1_page<TKey: Ord, TValue>(
    cache: &PageCache<Vec<(TKey, TValue)>>,
    interval: &SplitL0Interval<TKey>,
    load: impl Future<Output = Result<Vec<(TKey, TValue)>, ArchiveError>>,
) -> Result<Arc<Vec<(TKey, TValue)>>, ArchiveError> {
    let offset = interval.l1_offset as u64;

    let page = match cache.get(offset).await {
        Some(page) => page,
        None => cache.insert_within_capacity(offset, load.await?).await,
    };

    interval.check_spanned_by(
        page.first().map(|(key, _)| key),
        page.last().map(|(key, _)| key),
    )?;
    Ok(page)
}

/// What the L1 entry of `key` holds besides the key — where its data page is, in the preallocated modules:
/// `None` when no interval of `l0` holds it or its L1 page — asked of `page_of` — does not.
pub(crate) async fn split_data_ref_of<'a, TKey, TValue, TPage>(
    l0: &'a [SplitL0Interval<TKey>],
    key: &TKey,
    page_of: impl FnOnce(&'a SplitL0Interval<TKey>) -> TPage,
) -> Result<Option<TValue>, ArchiveError>
where
    TKey: Ord,
    TValue: Copy,
    TPage: Future<Output = Result<Arc<Vec<(TKey, TValue)>>, ArchiveError>>,
{
    let Some(interval) = split_l0_interval_of(l0, key) else {
        return Ok(None);
    };

    let page = page_of(interval).await?;

    Ok(page
        .binary_search_by(|(stored, _)| stored.cmp(key))
        .ok()
        .map(|at| page[at].1))
}

/// The `SLOTS × ITEM_LEN` body of the data page at `at` through `cache`: a hit, or on a miss the page read
/// by [`read_split_data_body`], inserted.
pub(crate) async fn split_data_body<TSlot, TItem, TReader, TFactory>(
    cache: &PageCache<Vec<u8>>,
    data_files: &LazyDataReaders<TReader>,
    factory: &TFactory,
    at: DataRef,
) -> Result<Arc<Vec<u8>>, ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TReader: ArchiveFileReader,
    TFactory: IoReaderFactory<TReader>,
{
    let key = at.cache_key();

    if let Some(body) = cache.get(key).await {
        return Ok(body);
    }

    let body = read_split_data_body::<TSlot, TItem, _, _>(data_files, factory, at).await?;
    Ok(cache.insert_within_capacity(key, body).await)
}

/// The `SLOTS × ITEM_LEN` body of the data page at `at`, read through no cache, its data file opened on
/// first use.
pub(crate) async fn read_split_data_body<TSlot, TItem, TReader, TFactory>(
    data_files: &LazyDataReaders<TReader>,
    factory: &TFactory,
    at: DataRef,
) -> Result<Vec<u8>, ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TReader: ArchiveFileReader,
    TFactory: IoReaderFactory<TReader>,
{
    let file = data_files.get(factory, at.file_no).await?;
    read_split_data_page::<TSlot, TItem, _>(file, at.offset).await
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;

    use tokio::sync::Notify;

    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader};
    use crate::split_test_io::{assert_still_waiting, poll_once, poll_until_parked};
    use crate::{
        ArchiveError, DataRef, LazyDataReaders, PageCache, SplitL0Interval, SplitL1Page, TinyItem,
        TinySlot, data_frame, split_data_body, split_data_ref_of, split_l1_page,
    };

    fn at(offset: u32) -> DataRef {
        DataRef { file_no: 0, offset }
    }

    fn interval(from: u32, to: u32, l1_offset: u32) -> SplitL0Interval<u32> {
        SplitL0Interval {
            from,
            to,
            l1_offset,
        }
    }

    /// A load that must never be needed.
    async fn unreachable_load() -> Result<SplitL1Page<u32>, ArchiveError> {
        panic!("a cached page was loaded again")
    }

    fn is_corrupted<T: std::fmt::Debug>(result: Result<T, ArchiveError>, case: &str) {
        match result {
            Err(ArchiveError::Corrupted(message)) => {
                assert!(message.contains("does not span"), "{case}: {message}")
            }
            other => panic!("{case}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cached_page_is_checked_against_the_interval_that_asks_for_it() {
        let cache = PageCache::new(4);
        let page = vec![(1, at(0)), (2, at(10))];

        let loaded = split_l1_page(&cache, &interval(1, 2, 7), async { Ok(page.clone()) })
            .await
            .unwrap();
        assert_eq!(*loaded, page);

        // A hit for the interval that loaded it.
        let hit = split_l1_page(&cache, &interval(1, 2, 7), unreachable_load())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&hit, &loaded));

        // A damaged L0 names the same page for another interval: the hit is refused, never answered.
        for (from, to) in [(3, 4), (1, 3), (0, 2)] {
            is_corrupted(
                split_l1_page(&cache, &interval(from, to, 7), unreachable_load()).await,
                &format!("{from}..={to}"),
            );
        }
    }

    #[tokio::test]
    async fn a_loaded_page_that_does_not_span_its_interval_is_refused_and_so_is_an_empty_one() {
        let cache = PageCache::new(4);

        is_corrupted(
            split_l1_page(&cache, &interval(1, 5, 9), async {
                Ok(vec![(1, at(0)), (4, at(10))])
            })
            .await,
            "short of `to`",
        );
        is_corrupted(
            split_l1_page(&cache, &interval(1, 5, 11), async { Ok(Vec::new()) }).await,
            "empty",
        );
    }

    #[tokio::test]
    async fn two_intervals_naming_one_page_are_an_error_for_get_warm_or_cold() {
        let l0 = [interval(1, 2, 7), interval(3, 4, 7)];
        let page = vec![(1, at(0)), (2, at(10))];

        for warm in [false, true] {
            let cache = PageCache::new(4);

            if warm {
                split_l1_page(&cache, &l0[0], async { Ok(page.clone()) })
                    .await
                    .unwrap();
            }

            let page_of = |interval| split_l1_page(&cache, interval, async { Ok(page.clone()) });

            is_corrupted(
                split_data_ref_of(&l0, &3, page_of).await,
                &format!("get, warm {warm}"),
            );
        }
    }

    /// The page of `interval` out of `pages`, which start at offset 7.
    fn load(
        pages: &[SplitL1Page<u32>],
        interval: &SplitL0Interval<u32>,
    ) -> impl Future<Output = Result<SplitL1Page<u32>, ArchiveError>> + use<> {
        let page = pages[(interval.l1_offset - 7) as usize].clone();
        async move { Ok(page) }
    }

    #[tokio::test]
    async fn a_key_is_found_on_the_page_of_its_interval_or_is_absent() {
        let l0 = [interval(1, 3, 7), interval(5, 8, 8)];
        let pages = [
            vec![(1, at(0)), (2, at(10)), (3, at(20))],
            vec![(5, at(30)), (8, at(40))],
        ];
        let cache = PageCache::new(4);

        let page_of = |interval| split_l1_page(&cache, interval, load(&pages, interval));

        assert_eq!(
            split_data_ref_of(&l0, &8, page_of).await.unwrap(),
            Some(at(40))
        );
        assert_eq!(
            split_data_ref_of(&l0, &2, page_of).await.unwrap(),
            Some(at(10))
        );
        assert_eq!(split_data_ref_of(&l0, &4, page_of).await.unwrap(), None);
        assert_eq!(split_data_ref_of(&l0, &6, page_of).await.unwrap(), None);
        assert_eq!(split_data_ref_of(&l0, &9, page_of).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_resize_landing_after_an_l1_page_is_loaded_and_before_it_is_inserted_bounds_the_cache()
     {
        let cache = PageCache::new(10);
        for (from, offset) in [(1, 7), (3, 8)] {
            split_l1_page(&cache, &interval(from, from, offset), async move {
                Ok(vec![(from, at(0))])
            })
            .await
            .unwrap();
        }

        let loaded = Arc::new(Notify::new());
        let load = {
            let loaded = loaded.clone();
            async move {
                loaded.notified().await;
                Ok(vec![(5, at(20))])
            }
        };

        let wanted = interval(5, 5, 9);
        let call = split_l1_page(&cache, &wanted, load);
        tokio::pin!(call);

        // Missed, and loading with no lock held.
        assert!(poll_once(call.as_mut()).await.is_pending());

        // Loaded, and parked at the insert — after an insert that reads the capacity before it locks has read
        // 10 — while the resize lands under the lock.
        let mut pages = cache.pages.write().await;
        loaded.notify_one();
        assert_still_waiting(call.as_mut()).await;

        cache.resize_locked(&mut pages, 1);
        assert_eq!(pages.len(), 1);
        drop(pages);

        assert_eq!(*call.await.unwrap(), vec![(5, at(20))]);
        assert_eq!(cache.pages.read().await.len(), 1, "above the capacity set");
        assert!(cache.get(9).await.is_some(), "the page just loaded is kept");
    }

    #[tokio::test]
    async fn a_resize_landing_after_a_data_page_is_read_and_before_it_is_inserted_bounds_the_cache()
    {
        let io = InMemoryIo::default();
        let page = |fill: u8| data_frame(22, 0, 4, &[fill; 12]);
        let file = io.put(FileRole::Data(0), [page(1), page(2), page(3)].concat());
        let readers = LazyDataReaders::<InMemoryReader>::new();

        let cache = PageCache::new(10);
        for offset in [0, 22] {
            split_data_body::<TinySlot, TinyItem, _, _>(&cache, &readers, &io, at(offset))
                .await
                .unwrap();
        }

        file.shut_reads_at(44);
        let call = split_data_body::<TinySlot, TinyItem, _, _>(&cache, &readers, &io, at(44));
        tokio::pin!(call);

        // Missed, and parked in its read with no lock held.
        poll_until_parked(call.as_mut(), || file.is_read_waiting()).await;

        let mut pages = cache.pages.write().await;
        file.open_reads();
        assert_still_waiting(call.as_mut()).await;

        cache.resize_locked(&mut pages, 1);
        drop(pages);

        assert_eq!(*call.await.unwrap(), vec![3; 12]);
        assert_eq!(cache.pages.read().await.len(), 1, "above the capacity set");
        assert!(cache.get(at(44).cache_key()).await.is_some());
    }
}
