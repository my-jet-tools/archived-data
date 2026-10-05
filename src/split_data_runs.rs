use crate::{
    ArchiveError, ArchiveFileReader, DataRef, IoReaderFactory, LazyDataReaders,
    PreallocatedDataItem, PreallocatedSlotKey, SPLIT_DATA_HEADER_LEN, SplitFrame, SplitFrameHeader,
    SplitFrameKind, decode_split_data_frames, read_split_data_page, split_data_page_len,
};

/// The most bytes one read of a run of data pages asks for. A run is held in memory whole and is one
/// request to an object store: 8 MiB is a couple of thousand pages of a few KiB, so a range of a compact
/// archive is a handful of requests, and no single read holds more than a bounded piece of the range.
pub(crate) const MAX_SPLIT_DATA_RUN_BYTES: usize = 8 * 1024 * 1024;

/// The `SLOTS × ITEM_LEN` body of the data page at each of `refs`, handed to `on_body` in the order of
/// `refs`, through no cache.
///
/// Refs that follow one another in the same data file — each page starting where the one before it ends
/// — are read with ONE `read_at` per run of at most [`MAX_SPLIT_DATA_RUN_BYTES`], clamped to the file.
/// Every page of a run is checked as a read of that page alone checks it, and a page chained to a
/// continuation is read on its own. The first damaged page is the `Err`, whatever was handed out before it.
pub(crate) async fn read_split_data_bodies<TSlot, TItem, TReader, TFactory>(
    data_files: &LazyDataReaders<TReader>,
    factory: &TFactory,
    refs: &[DataRef],
    mut on_body: impl FnMut(&[u8]),
) -> Result<(), ArchiveError>
where
    TSlot: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TReader: ArchiveFileReader,
    TFactory: IoReaderFactory<TReader>,
{
    let page_len = split_data_page_len::<TSlot, TItem>();
    let per_read = (MAX_SPLIT_DATA_RUN_BYTES / page_len).max(1);
    let mut start = 0;

    while start < refs.len() {
        let first = refs[start];
        let mut end = start + 1;

        while end < refs.len()
            && end - start < per_read
            && follows(refs[end - 1], refs[end], page_len)
        {
            end += 1;
        }

        let file = data_files.get(factory, first.file_no).await?;
        let run = file
            .read_up_to(first.offset as u64, (end - start) * page_len)
            .await?;

        for (index, at) in refs[start..end].iter().enumerate() {
            let within = index * page_len;
            let bytes = run
                .get(within..run.len().min(within + page_len))
                .unwrap_or_default();

            let header = SplitFrameHeader::decode(bytes, at.offset as u64, SplitFrameKind::Data)?;

            let whole = header.next == 0
                && header.size == page_len
                && header.field as usize == TItem::ITEM_LEN
                && bytes.len() == page_len;

            if whole {
                on_body(&bytes[SPLIT_DATA_HEADER_LEN..]);
            } else if header.next != 0 {
                on_body(&read_split_data_page::<TSlot, TItem, _>(file, at.offset).await?);
            } else {
                // Not a page of this type: decoded only for the error a read of the page alone gives.
                let frame =
                    SplitFrame::from_bytes(bytes.to_vec(), at.offset as u64, SplitFrameKind::Data)?;
                on_body(&decode_split_data_frames::<TSlot, TItem>(&[frame])?);
            }
        }

        start = end;
    }

    Ok(())
}

fn follows(previous: DataRef, next: DataRef, page_len: usize) -> bool {
    next.file_no == previous.file_no
        && next.offset as u64 == previous.offset as u64 + page_len as u64
}

#[cfg(test)]
mod tests {
    use crate::in_memory_io::{FileRole, InMemoryIo, InMemoryReader};
    use crate::{
        ArchiveError, DataRef, LazyDataReaders, MAX_SPLIT_DATA_RUN_BYTES, TinyItem, TinySlot,
        data_frame, read_split_data_bodies,
    };

    fn at(file_no: u8, offset: u32) -> DataRef {
        DataRef { file_no, offset }
    }

    fn page(value: u8) -> Vec<u8> {
        data_frame(22, 0, 4, &[value; 12])
    }

    async fn bodies(io: &InMemoryIo, refs: &[DataRef]) -> Result<Vec<Vec<u8>>, ArchiveError> {
        let files = LazyDataReaders::<InMemoryReader>::new();
        let mut bodies = Vec::new();

        read_split_data_bodies::<TinySlot, TinyItem, _, _>(&files, io, refs, |body| {
            bodies.push(body.to_vec())
        })
        .await?;

        Ok(bodies)
    }

    #[tokio::test]
    async fn pages_that_follow_one_another_are_one_read_and_come_back_in_ref_order() {
        let io = InMemoryIo::default();
        let data_0 = io.put(FileRole::Data(0), [page(1), page(2), page(3)].concat());
        let data_1 = io.put(FileRole::Data(1), [page(4), page(5)].concat());

        // 0 and 22 follow; 66 does not follow 22 because 44 is skipped; file 1 is a run of its own.
        let refs = [at(0, 0), at(0, 22), at(1, 0), at(1, 22), at(0, 44)];
        let got = bodies(&io, &refs).await.unwrap();

        assert_eq!(
            got,
            [[1; 12], [2; 12], [4; 12], [5; 12], [3; 12]].map(Vec::from)
        );
        assert_eq!((data_0.read_ats(), data_1.read_ats()), (2, 1));
        assert_eq!(bodies(&io, &[]).await.unwrap(), Vec::<Vec<u8>>::new());
    }

    #[tokio::test]
    async fn a_run_is_cut_at_the_read_limit() {
        let pages = MAX_SPLIT_DATA_RUN_BYTES / 22 + 1;
        let io = InMemoryIo::default();
        let data_0 = io.put(FileRole::Data(0), page(9).repeat(pages));

        let refs: Vec<DataRef> = (0..pages).map(|n| at(0, (n * 22) as u32)).collect();
        assert_eq!(bodies(&io, &refs).await.unwrap().len(), pages);
        assert_eq!(data_0.read_ats(), 2);
    }

    #[tokio::test]
    async fn a_chained_page_in_a_run_is_read_on_its_own() {
        let io = InMemoryIo::default();
        let mut bytes = page(1);
        bytes.extend_from_slice(&data_frame(18, 44, 4, &[2; 8]));
        bytes.resize(44, 0xCC);
        bytes.extend_from_slice(&data_frame(14, 0, 4, &[2; 4]));
        let data_0 = io.put(FileRole::Data(0), bytes);

        // The run reads 0..44; the page at 22 is a chain, read again on its own: two frames, two reads.
        let got = bodies(&io, &[at(0, 0), at(0, 22)]).await.unwrap();
        assert_eq!(got, [vec![1; 12], vec![2; 12]]);
        assert_eq!(data_0.read_ats(), 1 + 2);
    }

    #[tokio::test]
    async fn a_damaged_page_anywhere_in_a_run_is_corrupted() {
        let whole = [page(1), page(2), page(3)].concat();

        let mut item_len = whole.clone();
        item_len[22 + 8] = 5;
        let mut size = whole.clone();
        size[22] = 21;
        let cut = whole[..60].to_vec();

        for bytes in [item_len, size, cut] {
            let io = InMemoryIo::default();
            io.put(FileRole::Data(0), bytes);

            let refs = [at(0, 0), at(0, 22), at(0, 44)];
            assert!(matches!(
                bodies(&io, &refs).await,
                Err(ArchiveError::Corrupted(_))
            ));
        }

        let io = InMemoryIo::default();
        let file = io.put(FileRole::Data(0), whole);
        assert!(matches!(
            bodies(&io, &[at(0, 44), at(0, 66)]).await,
            Err(ArchiveError::Corrupted(_))
        ));
        assert_eq!(file.eof_reads(), 0);
    }
}
