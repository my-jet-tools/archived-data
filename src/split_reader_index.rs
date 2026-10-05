//! A split reader's side of the index before and beside the keys: opening it — L0 read and checked, and an
//! index cut short refused — and counting the keys its L1 pages hold. What an entry looks like is the
//! module's, handed in as a function.

use crate::{
    ArchiveError, ArchiveFileReader, IoReaderFactory, SizedReader, SplitFrame, SplitFrameHeader,
    SplitFrameKind, SplitL0Interval, read_split_header_chain, read_split_l0_chain,
    refuse_l1_offsets_past_the_index,
};

/// Opens the index and reads its L0 chain, decoded by the module's `decode`, and returns the index, L0 and
/// how many frames L0 was read from. Refused as `Corrupted`, besides whatever `decode` refuses: an L0 header
/// that fails its hash, a broken chain, an L1 page whose header lies past the end of the index (checked
/// with no read), and an index cut inside its last L1 page (one small read, see
/// [`refuse_an_index_cut_inside_its_last_l1_page`]).
pub(crate) async fn open_split_index<TKey, TFactory, TReader>(
    factory: &TFactory,
    decode: impl FnOnce(&[SplitFrame]) -> Result<Vec<SplitL0Interval<TKey>>, ArchiveError>,
) -> Result<(SizedReader<TReader>, Vec<SplitL0Interval<TKey>>, usize), ArchiveError>
where
    TFactory: IoReaderFactory<TReader>,
    TReader: ArchiveFileReader,
{
    let index = SizedReader::open(factory.create_index_reader().await?).await?;
    let frames = read_split_l0_chain(&index).await?;
    let l0 = decode(&frames)?;

    let l1_offsets = || l0.iter().map(|interval| interval.l1_offset);
    refuse_l1_offsets_past_the_index(l1_offsets(), index.size())?;
    refuse_an_index_cut_inside_its_last_l1_page(&index, l1_offsets()).await?;

    Ok((index, l0, frames.len()))
}

/// Refuses an index that ends inside its LAST L1 page: the header of the page at the highest `l1_offset`
/// is read — one `read_at` of 12 bytes, and one more per continuation it chains to — and that page, every
/// frame of it, must end inside the index. An empty L0 costs no read.
///
/// Every index grows only by append, and a compaction appends its L1 pages in offset order, so a cut can
/// only fall in the page at the highest offset; an index cut before that page's header is refused already,
/// with no read, by [`refuse_l1_offsets_past_the_index`]. A file writer whose append fails partway, and is
/// then dropped, persists the part that reached it: without this check such an index would open, and every
/// key of its last page would fail when read.
pub(crate) async fn refuse_an_index_cut_inside_its_last_l1_page<TReader: ArchiveFileReader>(
    index: &SizedReader<TReader>,
    l1_offsets: impl IntoIterator<Item = u32>,
) -> Result<(), ArchiveError> {
    let Some(last) = l1_offsets.into_iter().max() else {
        return Ok(());
    };

    read_split_header_chain(index, last as u64, SplitFrameKind::L1).await?;
    Ok(())
}

/// The sum of the `count` of every L1 frame header, each checked by the module's `check`: one small read
/// per L1 frame, uncached.
pub(crate) async fn count_split_l1_keys<TKey, TReader: ArchiveFileReader>(
    index: &SizedReader<TReader>,
    l0: &[SplitL0Interval<TKey>],
    check: impl Fn(&SplitFrameHeader) -> Result<usize, ArchiveError>,
) -> Result<usize, ArchiveError> {
    let mut total = 0;

    for interval in l0 {
        let headers =
            read_split_header_chain(index, interval.l1_offset as u64, SplitFrameKind::L1).await?;

        for header in &headers {
            total += check(header)?;
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader};
    use crate::{
        ArchiveError, SizedReader, SplitFrame, SplitL0Interval, l0_frame, l1_frame, le_u32,
        open_split_index,
    };

    const L0_LEN: usize = 20 + 2 * 12;
    const PAGE_A: usize = L0_LEN;
    const PAGE_LEN: usize = 12 + 18;
    const PAGE_B: usize = PAGE_A + PAGE_LEN;

    /// `from u32 · to u32 · l1_offset u32` per interval: an L0 of this test's own.
    fn decode(frames: &[SplitFrame]) -> Result<Vec<SplitL0Interval<u32>>, ArchiveError> {
        let mut l0 = Vec::new();

        for frame in frames {
            for entry in 0..frame.header.field as usize {
                let at = entry * 12;
                l0.push(SplitL0Interval {
                    from: le_u32(&frame.body, at),
                    to: le_u32(&frame.body, at + 4),
                    l1_offset: le_u32(&frame.body, at + 8),
                });
            }
        }

        Ok(l0)
    }

    fn interval(key: u32, l1_offset: usize) -> Vec<u8> {
        [key, key, l1_offset as u32]
            .iter()
            .flat_map(|field| field.to_le_bytes())
            .collect()
    }

    /// L0 naming page A then page B, the two pages back to back: what a compaction appends.
    fn index() -> Vec<u8> {
        let mut index = l0_frame(
            L0_LEN as u32,
            0,
            2,
            &[interval(1, PAGE_A), interval(2, PAGE_B)].concat(),
        );
        index.extend_from_slice(&l1_frame(PAGE_LEN as u32, 0, 1, &[1; 18]));
        index.extend_from_slice(&l1_frame(PAGE_LEN as u32, 0, 1, &[2; 18]));
        index
    }

    async fn open(bytes: Vec<u8>) -> (Result<usize, ArchiveError>, usize) {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Index, bytes);

        let opened = open_split_index::<u32, InMemoryIo, InMemoryReader>(&io, decode)
            .await
            .map(|(_, l0, _): (SizedReader<InMemoryReader>, _, _)| l0.len());

        (opened, file.read_ats())
    }

    fn assert_cut(opened: Result<usize, ArchiveError>, case: &str) {
        match opened {
            Err(ArchiveError::Corrupted(message)) => {
                assert!(message.contains("past the end"), "{case}: {message}")
            }
            other => panic!("{case}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_intact_index_costs_one_more_read_than_its_l0() {
        let (opened, read_ats) = open(index()).await;

        assert_eq!(opened.unwrap(), 2);
        assert_eq!(read_ats, 2);
    }

    #[tokio::test]
    async fn an_index_cut_anywhere_inside_its_last_l1_page_is_refused_at_open() {
        let full = index();

        for cut in PAGE_B..full.len() {
            let (opened, read_ats) = open(full[..cut].to_vec()).await;
            assert_cut(opened, &format!("cut at {cut}"));

            // Short of the header, no read is needed to see it; past it, the header is read once.
            let expected = if cut < PAGE_B + 12 { 1 } else { 2 };
            assert_eq!(read_ats, expected, "cut at {cut}");
        }
    }

    #[tokio::test]
    async fn the_page_at_the_highest_offset_is_checked_whatever_order_l0_names_it_in() {
        // Page B lies first in the file here, page A last: L0 order is key order, not file order.
        let mut index = l0_frame(
            L0_LEN as u32,
            0,
            2,
            &[interval(1, PAGE_B), interval(2, PAGE_A)].concat(),
        );
        index.extend_from_slice(&l1_frame(PAGE_LEN as u32, 0, 1, &[2; 18]));
        index.extend_from_slice(&l1_frame(PAGE_LEN as u32, 0, 1, &[1; 18]));

        assert_eq!(open(index.clone()).await.0.unwrap(), 2);

        let (opened, _) = open(index[..index.len() - 1].to_vec()).await;
        assert_cut(opened, "the page at the highest offset cut by one byte");
    }

    #[tokio::test]
    async fn every_frame_of_a_chained_last_page_must_end_inside_the_index() {
        // Page B chains to a continuation appended after it.
        let continuation = PAGE_B + PAGE_LEN;
        let mut chained = index();
        chained[PAGE_B + 4..PAGE_B + 8].copy_from_slice(&(continuation as u32).to_le_bytes());
        chained.extend_from_slice(&l1_frame(PAGE_LEN as u32, 0, 1, &[3; 18]));

        let (opened, read_ats) = open(chained.clone()).await;
        assert_eq!(opened.unwrap(), 2);
        assert_eq!(
            read_ats, 3,
            "L0, and the header of each frame of the last page"
        );

        for cut in [continuation + 12, chained.len() - 1] {
            let (opened, _) = open(chained[..cut].to_vec()).await;
            assert_cut(opened, &format!("the continuation cut at {cut}"));
        }
    }

    #[tokio::test]
    async fn an_empty_l0_costs_no_read_past_it() {
        let (opened, read_ats) = open(l0_frame(20, 0, 0, &[])).await;

        assert_eq!(opened.unwrap(), 0);
        assert_eq!(read_ats, 1);
    }
}
