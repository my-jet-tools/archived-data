//! The L1 side of a split reader's `get_range`: which intervals a range touches, and their pages read in
//! RUNS. Nothing here knows how a page's entries decode: a module hands in its decode of the frames of one
//! page, and gets back the same walk in every module.

use std::sync::Arc;

use crate::{
    ArchiveError, ArchiveFileReader, MAX_SPLIT_DATA_RUN_BYTES, PageCache, SizedReader, SplitFrame,
    SplitFrameHeader, SplitFrameKind, SplitL0Interval, SplitL1Page, read_split_chain,
    split_l1_page,
};

/// The L1 entries of every key `k` with `from <= k <= to`, in key order: the intervals of `l0` that can
/// hold such a key are found in memory, their pages are read by [`read_split_range_l1_pages`], and each
/// page is walked in memory from its first key `>= from` while keys are `<= to`.
///
/// `page_guess` is how many bytes a load of one page asks for before its header is known, and `decode`
/// turns the frames of one page into its entries, checking them as a load of that page checks them.
pub(crate) async fn split_entries_between<TKey, TReader>(
    index: &SizedReader<TReader>,
    cache: &PageCache<SplitL1Page<TKey>>,
    l0: &[SplitL0Interval<TKey>],
    from: &TKey,
    to: &TKey,
    page_guess: usize,
    decode: impl Fn(&[SplitFrame], &SplitL0Interval<TKey>) -> Result<SplitL1Page<TKey>, ArchiveError>,
) -> Result<SplitL1Page<TKey>, ArchiveError>
where
    TKey: Ord + Clone,
    TReader: ArchiveFileReader,
{
    let first = l0.partition_point(|interval| interval.to < *from);
    let intervals = &l0[first..];
    let intervals = &intervals[..intervals.partition_point(|interval| interval.from <= *to)];

    let mut entries = Vec::new();

    read_split_range_l1_pages(index, cache, intervals, page_guess, &decode, |page| {
        let start = page.partition_point(|(key, _)| key < from);

        entries.extend(
            page[start..]
                .iter()
                .take_while(|(key, _)| key <= to)
                .cloned(),
        );
    })
    .await?;

    Ok(entries)
}

/// The L1 page of every one of `intervals`, handed to `on_page` in their order, each checked to span its
/// interval exactly — a cached page included — and every page read inserted into `cache`.
///
/// * A page already in the cache comes from it, reading nothing.
/// * Consecutive intervals whose pages are NOT cached and lie one after another in the index are read with
///   ONE `read_at` per run, of at most [`MAX_SPLIT_DATA_RUN_BYTES`], clamped to the index. A page's size is
///   known only once its header is read, so a page is taken to follow the one before it when it starts
///   after it by at most `page_guess` bytes — in a compact index, exactly the pages back to back. The read
///   reaches from the first page of the run to `page_guess` bytes past the start of its last.
/// * Every page of a run that lies in the run's bytes as ONE frame is decoded out of them by `decode`,
///   exactly as a load of that page alone decodes it. A page chained to a continuation, one that reaches
///   past the run's bytes, and one whose header does not decode are loaded on their own instead, so what
///   they answer — the page, or the error — is what a load of that page alone answers.
/// * A page with no neighbour to share a read with is loaded on its own: one `read_at` of `page_guess`
///   bytes, then its continuations.
///
/// The first page that fails — a read, a decode, a span — is the `Err`, whatever was handed out before it.
pub(crate) async fn read_split_range_l1_pages<TKey, TReader>(
    index: &SizedReader<TReader>,
    cache: &PageCache<SplitL1Page<TKey>>,
    intervals: &[SplitL0Interval<TKey>],
    page_guess: usize,
    decode: &impl Fn(&[SplitFrame], &SplitL0Interval<TKey>) -> Result<SplitL1Page<TKey>, ArchiveError>,
    mut on_page: impl FnMut(&SplitL1Page<TKey>),
) -> Result<(), ArchiveError>
where
    TKey: Ord,
    TReader: ArchiveFileReader,
{
    let mut cached = Vec::with_capacity(intervals.len());

    for interval in intervals {
        cached.push(cache.get(interval.l1_offset as u64).await);
    }

    let mut start = 0;

    while start < intervals.len() {
        if let Some(page) = cached[start].take() {
            let page = spanning(&intervals[start], page)?;
            on_page(&page);
            start += 1;
            continue;
        }

        let end = end_of_uncached_run(intervals, &cached, start, page_guess);

        if end == start + 1 {
            let page = load(index, cache, &intervals[start], page_guess, decode).await?;
            on_page(&page);
            start = end;
            continue;
        }

        let first = intervals[start].l1_offset as u64;
        let last = intervals[end - 1].l1_offset as u64;
        let run = index
            .read_up_to(first, (last - first) as usize + page_guess)
            .await?;

        for interval in &intervals[start..end] {
            let offset = interval.l1_offset as u64;
            let within = (offset - first) as usize;

            let page = match one_frame_at(&run, within, offset) {
                Some(frame) => {
                    let page = decode(&[frame], interval)?;
                    spanning(interval, cache.insert_within_capacity(offset, page).await)?
                }
                None => load(index, cache, interval, page_guess, decode).await?,
            };

            on_page(&page);
        }

        start = end;
    }

    Ok(())
}

/// The end of the run of uncached pages that starts at `start`: every next interval's page is not cached,
/// starts after the page before it by at most `page_guess` bytes, and still fits one read of at most
/// [`MAX_SPLIT_DATA_RUN_BYTES`] reaching `page_guess` bytes past its start.
fn end_of_uncached_run<TKey, TPage>(
    intervals: &[SplitL0Interval<TKey>],
    cached: &[Option<TPage>],
    start: usize,
    page_guess: usize,
) -> usize {
    let first = intervals[start].l1_offset as u64;
    let guess = page_guess as u64;
    let mut end = start + 1;

    while end < intervals.len() && cached[end].is_none() {
        let previous = intervals[end - 1].l1_offset as u64;
        let next = intervals[end].l1_offset as u64;

        // `previous >= first`, so once `next` follows `previous` it is past `first` too.
        let follows = next > previous && next - previous <= guess;

        if !follows || next - first + guess > MAX_SPLIT_DATA_RUN_BYTES as u64 {
            break;
        }

        end += 1;
    }

    end
}

/// The frame at `within` of bytes read from `offset - within`, when it lies there whole and chains to
/// nothing; `None` for anything a load of the page alone must read, or refuse, itself.
fn one_frame_at(run: &[u8], within: usize, offset: u64) -> Option<SplitFrame> {
    let bytes = run.get(within..)?;
    let header = SplitFrameHeader::decode(bytes, offset, SplitFrameKind::L1).ok()?;

    if header.next != 0 || header.size > bytes.len() {
        return None;
    }

    SplitFrame::from_bytes(bytes[..header.size].to_vec(), offset, SplitFrameKind::L1).ok()
}

/// The page of `interval` through the cache, loaded on a miss as a reader loads one page: its first frame in
/// one read of `page_guess` bytes, then every continuation.
async fn load<TKey, TReader>(
    index: &SizedReader<TReader>,
    cache: &PageCache<SplitL1Page<TKey>>,
    interval: &SplitL0Interval<TKey>,
    page_guess: usize,
    decode: &impl Fn(&[SplitFrame], &SplitL0Interval<TKey>) -> Result<SplitL1Page<TKey>, ArchiveError>,
) -> Result<Arc<SplitL1Page<TKey>>, ArchiveError>
where
    TKey: Ord,
    TReader: ArchiveFileReader,
{
    let read = async {
        let frames = read_split_chain(
            index,
            interval.l1_offset as u64,
            SplitFrameKind::L1,
            page_guess,
        )
        .await?;

        decode(&frames, interval)
    };

    split_l1_page(cache, interval, read).await
}

fn spanning<TKey: Ord>(
    interval: &SplitL0Interval<TKey>,
    page: Arc<SplitL1Page<TKey>>,
) -> Result<Arc<SplitL1Page<TKey>>, ArchiveError> {
    interval.check_spanned_by(
        page.first().map(|(key, _)| key),
        page.last().map(|(key, _)| key),
    )?;

    Ok(page)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::in_memory_io::{FileRole, InMemoryFile, InMemoryIo, InMemoryReader};
    use crate::{
        ArchiveError, DataRef, IoReaderFactory, MAX_SPLIT_DATA_RUN_BYTES, PageCache, SizedReader,
        SplitFrame, SplitL0Interval, SplitL1Page, l1_frame, le_u32, read_split_range_l1_pages,
        split_entries_between,
    };

    /// `key u32` per entry, each key's data page at `key × 10`: an L1 page of this test's own.
    fn decode(
        frames: &[SplitFrame],
        interval: &SplitL0Interval<u32>,
    ) -> Result<SplitL1Page<u32>, ArchiveError> {
        let mut page = Vec::new();

        for frame in frames {
            let count = frame.header.field as usize;

            if count == 0 || count * 4 > frame.body.len() {
                return Err(ArchiveError::Corrupted(format!(
                    "the L1 frame at {} declares count {count}",
                    frame.header.offset
                )));
            }

            for entry in 0..count {
                let key = le_u32(&frame.body, entry * 4);
                page.push((
                    key,
                    DataRef {
                        file_no: 0,
                        offset: key * 10,
                    },
                ));
            }
        }

        interval.check_spanned_by(
            page.first().map(|(key, _)| key),
            page.last().map(|(key, _)| key),
        )?;

        Ok(page)
    }

    /// One L1 frame of `size` bytes holding `keys`.
    fn page(size: usize, keys: &[u32]) -> Vec<u8> {
        let body: Vec<u8> = keys.iter().flat_map(|key| key.to_le_bytes()).collect();
        l1_frame(size as u32, 0, keys.len() as u32, &body)
    }

    /// An index of `pages` — each `(offset, keys)`, frames of `PAGE` bytes — with filler in any gap, and the
    /// intervals naming them in key order.
    fn index(pages: &[(usize, Vec<u32>)]) -> (Vec<u8>, Vec<SplitL0Interval<u32>>) {
        let mut bytes = Vec::new();
        let mut by_offset: Vec<&(usize, Vec<u32>)> = pages.iter().collect();
        by_offset.sort_by_key(|(offset, _)| *offset);

        for (offset, keys) in by_offset {
            bytes.resize(*offset, 0xCC);
            bytes.extend_from_slice(&page(PAGE, keys));
        }

        let intervals = pages
            .iter()
            .map(|(offset, keys)| SplitL0Interval {
                from: keys[0],
                to: keys[keys.len() - 1],
                l1_offset: *offset as u32,
            })
            .collect();

        (bytes, intervals)
    }

    const PAGE: usize = 12 + 3 * 4;

    /// Five pages back to back from offset 20, three keys each: 10..=12, 20..=22, … 50..=52.
    fn five_back_to_back() -> (Vec<u8>, Vec<SplitL0Interval<u32>>) {
        let pages: Vec<(usize, Vec<u32>)> = (0..5)
            .map(|n| {
                let first = (n as u32 + 1) * 10;
                (20 + n * PAGE, vec![first, first + 1, first + 2])
            })
            .collect();

        index(&pages)
    }

    async fn open(bytes: Vec<u8>) -> (SizedReader<InMemoryReader>, Arc<InMemoryFile>) {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Index, bytes);
        let reader = SizedReader::open(io.create_index_reader().await.unwrap())
            .await
            .unwrap();

        (reader, file)
    }

    /// The keys of every page read for `intervals`, as `on_page` saw them.
    async fn pages_of(
        reader: &SizedReader<InMemoryReader>,
        cache: &PageCache<SplitL1Page<u32>>,
        intervals: &[SplitL0Interval<u32>],
        guess: usize,
    ) -> Result<Vec<Vec<u32>>, ArchiveError> {
        let mut seen = Vec::new();

        read_split_range_l1_pages(reader, cache, intervals, guess, &decode, |page| {
            seen.push(page.iter().map(|(key, _)| *key).collect())
        })
        .await?;

        Ok(seen)
    }

    fn keys(intervals: &[SplitL0Interval<u32>]) -> Vec<Vec<u32>> {
        intervals
            .iter()
            .map(|interval| (interval.from..=interval.to).collect())
            .collect()
    }

    async fn cached_offsets(cache: &PageCache<SplitL1Page<u32>>) -> Vec<u64> {
        let mut offsets: Vec<u64> = cache.pages.read().await.keys().copied().collect();
        offsets.sort_unstable();
        offsets
    }

    fn is_corrupted<T: std::fmt::Debug>(result: Result<T, ArchiveError>, needle: &str, case: &str) {
        match result {
            Err(ArchiveError::Corrupted(message)) => {
                assert!(message.contains(needle), "{case}: {message}")
            }
            other => panic!("{case}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn pages_back_to_back_are_one_read_then_come_from_the_cache() {
        let (bytes, intervals) = five_back_to_back();
        let (reader, file) = open(bytes).await;
        let cache = PageCache::new(10);

        assert_eq!(
            pages_of(&reader, &cache, &intervals, PAGE).await.unwrap(),
            keys(&intervals)
        );
        assert_eq!(file.read_ats(), 1);
        assert_eq!(file.eof_reads(), 0);
        assert_eq!(
            cached_offsets(&cache).await,
            (0..5).map(|n| (20 + n * PAGE) as u64).collect::<Vec<_>>()
        );

        assert_eq!(
            pages_of(&reader, &cache, &intervals, PAGE).await.unwrap(),
            keys(&intervals)
        );
        assert_eq!(file.read_ats(), 1, "a warm cache reads nothing");
    }

    #[tokio::test]
    async fn a_cached_page_splits_the_run_and_a_lone_page_is_its_own_load() {
        let (bytes, intervals) = five_back_to_back();
        let (reader, file) = open(bytes).await;
        let cache = PageCache::new(10);

        // Page 1 cached: page 0 alone, then pages 2..=4 together.
        pages_of(&reader, &cache, &intervals[1..2], PAGE)
            .await
            .unwrap();
        assert_eq!(file.read_ats(), 1);

        assert_eq!(
            pages_of(&reader, &cache, &intervals, PAGE).await.unwrap(),
            keys(&intervals)
        );
        assert_eq!(file.read_ats(), 1 + 2);
        assert_eq!(cached_offsets(&cache).await.len(), 5);
    }

    #[tokio::test]
    async fn pages_that_do_not_follow_one_another_are_read_one_by_one() {
        // Key order is not file order, and every other gap is one byte longer than the guess.
        let (bytes, intervals) = index(&[
            (100, vec![1, 2, 3]),
            (20, vec![4, 5, 6]),
            (21 + PAGE, vec![7, 8, 9]),
            (22 + 2 * PAGE, vec![10, 11, 12]),
        ]);
        let (reader, file) = open(bytes).await;
        let cache = PageCache::new(0);

        assert_eq!(
            pages_of(&reader, &cache, &intervals, PAGE).await.unwrap(),
            keys(&intervals)
        );
        assert_eq!(file.read_ats(), 4);
        assert!(cached_offsets(&cache).await.is_empty(), "a cache of 0");

        // One byte more of guess, and the last three share a read: 100 alone, then one run of three.
        let (reader, file) = open(file.bytes().await).await;
        assert_eq!(
            pages_of(&reader, &cache, &intervals, PAGE + 1)
                .await
                .unwrap(),
            keys(&intervals)
        );
        assert_eq!(file.read_ats(), 2);
        assert_eq!(file.eof_reads(), 0);
    }

    #[tokio::test]
    async fn a_run_stops_where_one_read_would_pass_its_limit() {
        let (bytes, intervals) = five_back_to_back();
        let (reader, file) = open(bytes).await;

        // A guess so long that a run of more than two pages would ask for more than the limit.
        let guess = MAX_SPLIT_DATA_RUN_BYTES - PAGE;

        assert_eq!(
            pages_of(&reader, &PageCache::new(0), &intervals, guess)
                .await
                .unwrap(),
            keys(&intervals)
        );
        assert_eq!(file.read_ats(), 3, "pages 0-1, 2-3, then 4 alone");
        assert_eq!(file.eof_reads(), 0);
    }

    #[tokio::test]
    async fn a_chained_page_or_one_past_the_run_is_loaded_on_its_own() {
        let (_, intervals) = five_back_to_back();

        // The offsets of `five_back_to_back`, but page 1 chains its last key to a continuation at the end,
        // and page 4 is longer than the guess.
        let continuation = 20 + 4 * PAGE + PAGE + 8;
        let mut chained = page(PAGE, &[20, 21]);
        chained[4..8].copy_from_slice(&(continuation as u32).to_le_bytes());

        let mut bytes = vec![0xCC; 20];
        bytes.extend_from_slice(&page(PAGE, &[10, 11, 12]));
        bytes.extend_from_slice(&chained);
        bytes.extend_from_slice(&page(PAGE, &[30, 31, 32]));
        bytes.extend_from_slice(&page(PAGE, &[40, 41, 42]));
        bytes.extend_from_slice(&page(PAGE + 8, &[50, 51, 52]));
        bytes.extend_from_slice(&page(PAGE, &[22]));
        assert_eq!(bytes.len(), continuation + PAGE);

        let (reader, file) = open(bytes).await;

        assert_eq!(
            pages_of(&reader, &PageCache::new(10), &intervals, PAGE)
                .await
                .unwrap(),
            keys(&intervals)
        );
        // The run; page 1's two frames; page 4's first bytes, then the rest of its frame.
        assert_eq!(file.read_ats(), 1 + 2 + 2);
        assert_eq!(file.eof_reads(), 0);
    }

    #[tokio::test]
    async fn a_damaged_page_in_a_run_answers_what_its_own_load_answers() {
        type Damage = (&'static str, usize, u32, &'static str);

        let damages: [Damage; 3] = [
            ("a count of 0", 8, 0, "declares count 0"),
            ("a size past the end", 0, 9_999, "past the end"),
            (
                "a size shorter than a header",
                0,
                11,
                "shorter than its header",
            ),
        ];

        for (case, field, value, needle) in damages {
            let (mut bytes, intervals) = five_back_to_back();
            let at = 20 + 2 * PAGE + field;
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());

            let (reader, _) = open(bytes.clone()).await;
            is_corrupted(
                pages_of(&reader, &PageCache::new(10), &intervals, PAGE).await,
                needle,
                case,
            );

            let (reader, _) = open(bytes).await;
            is_corrupted(
                pages_of(&reader, &PageCache::new(10), &intervals[2..3], PAGE).await,
                needle,
                &format!("{case}, alone"),
            );
        }

        let (bytes, intervals) = five_back_to_back();
        let (reader, file) = open(bytes).await;
        file.fail_reads();
        assert!(matches!(
            pages_of(&reader, &PageCache::new(10), &intervals, PAGE).await,
            Err(ArchiveError::Io(_))
        ));
    }

    #[tokio::test]
    async fn two_intervals_naming_one_page_are_an_error_for_a_range_warm_or_cold() {
        let (bytes, mut intervals) = five_back_to_back();
        intervals[3].l1_offset = intervals[1].l1_offset;

        for warm in [false, true] {
            let (reader, _) = open(bytes.clone()).await;
            let cache = PageCache::new(10);

            if warm {
                pages_of(&reader, &cache, &intervals[1..2], PAGE)
                    .await
                    .unwrap();
            }

            is_corrupted(
                split_entries_between(&reader, &cache, &intervals, &0, &100, PAGE, decode).await,
                "does not span",
                &format!("warm {warm}"),
            );
        }
    }

    #[tokio::test]
    async fn a_range_takes_every_key_between_its_bounds_across_pages() {
        let (bytes, intervals) = five_back_to_back();
        let (reader, file) = open(bytes).await;
        let cache = PageCache::new(10);

        let keys = |entries: SplitL1Page<u32>| -> Vec<u32> {
            entries.into_iter().map(|(key, _)| key).collect()
        };

        for (from, to, expected, pages) in [
            (
                0,
                100,
                vec![10, 11, 12, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52],
                5,
            ),
            (11, 30, vec![11, 12, 20, 21, 22, 30], 3),
            (13, 19, vec![], 0),
            (41, 45, vec![41, 42], 1),
            (53, 200, vec![], 0),
        ] {
            let cache = PageCache::new(0);
            let reads = file.read_ats();

            let entries =
                split_entries_between(&reader, &cache, &intervals, &from, &to, PAGE, decode)
                    .await
                    .unwrap();
            assert_eq!(keys(entries), expected, "{from}..={to}");
            assert_eq!(file.read_ats() - reads, pages.min(1), "{from}..={to}");
        }

        let entries = split_entries_between(&reader, &cache, &intervals, &21, &21, PAGE, decode)
            .await
            .unwrap();
        assert_eq!(
            entries,
            [(
                21,
                DataRef {
                    file_no: 0,
                    offset: 210
                }
            )]
        );
        assert_eq!(cached_offsets(&cache).await, [(20 + PAGE) as u64]);
    }

    #[tokio::test]
    async fn a_resize_racing_the_insert_of_a_run_page_leaves_the_cache_within_it() {
        use crate::split_test_io::{assert_still_waiting, poll_once, poll_until_parked};

        let (bytes, intervals) = index(&[(20, vec![10, 11, 12]), (20 + PAGE, vec![20, 21, 22])]);
        let (reader, file) = open(bytes).await;
        let cache: PageCache<SplitL1Page<u32>> = PageCache::new(10);
        cache
            .insert_within_capacity(
                900,
                vec![(
                    1,
                    DataRef {
                        file_no: 0,
                        offset: 0,
                    },
                )],
            )
            .await;
        cache
            .insert_within_capacity(
                901,
                vec![(
                    2,
                    DataRef {
                        file_no: 0,
                        offset: 0,
                    },
                )],
            )
            .await;

        file.shut_reads_at(20);
        let call = pages_of(&reader, &cache, &intervals, PAGE);
        tokio::pin!(call);
        poll_until_parked(call.as_mut(), || file.is_read_waiting()).await;

        // The run is read; page A waits at its insert under the lock the test holds, and the resize to 1 lands.
        let mut w1 = cache.pages.write().await;
        file.open_reads();
        assert_still_waiting(call.as_mut()).await;
        cache.resize_locked(&mut w1, 1);
        drop(w1);

        // The lock queued next for the test: A inserts, then page B waits at its insert behind it.
        let w2 = cache.pages.write();
        tokio::pin!(w2);
        assert!(poll_once(w2.as_mut()).await.is_pending());
        assert!(poll_once(call.as_mut()).await.is_pending());
        let after_a = match poll_once(w2.as_mut()).await {
            std::task::Poll::Ready(mut w2) => {
                let len = w2.len();
                cache.resize_locked(&mut w2, 0);
                len
            }
            std::task::Poll::Pending => panic!("the test's lock did not come next"),
        };

        assert_eq!(
            call.await.unwrap(),
            vec![vec![10, 11, 12], vec![20, 21, 22]]
        );
        let end = cache.pages.read().await.len();
        assert!(after_a <= 1, "above capacity 1 after A: {after_a}");
        assert_eq!(end, 0, "above capacity 0 after B");
    }
}
