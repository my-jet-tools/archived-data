use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, AsyncWriteExt};

use crate::{
    ArchiveError, COMPACT_L1_CAPACITY, FixedKeyPreallocatedSlotsArchiveReaderSingleFile,
    L0_HEADER_LEN, L0Entry, PreallocatedDataItem, PreallocatedSlotKey, PrimaryKeyFixedSize,
    data_page_len, decode_l1_page, encode_l0_bodies, encode_l0_frame, encode_l1_page, l0_frame_len,
    l1_page_len,
};

impl<TPrimaryKey, TSlotKey, TItem, TSource>
    FixedKeyPreallocatedSlotsArchiveReaderSingleFile<TPrimaryKey, TSlotKey, TItem, TSource>
where
    TPrimaryKey: PrimaryKeyFixedSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
    TSource: AsyncRead + AsyncSeek + Unpin,
{
    /// The exact length [`Self::write_compacted_to`] writes, from the L1 headers alone.
    pub async fn compacted_len(&self) -> Result<u64, ArchiveError> {
        let keys = self.keys_count().await?;
        Ok(compact_len::<TPrimaryKey, TSlotKey, TItem>(keys) as u64)
    }

    /// Writes the COMPACT file to `out`, from its first byte, and returns the length written:
    ///
    /// ONE L0 frame (`next` 0) with exactly `ceil(n / COMPACT_L1_CAPACITY)` intervals · full L1 pages of
    /// [`COMPACT_L1_CAPACITY`] keys, the last zero-padded · every data page copied whole in ascending
    /// key order · all offsets absolute. The bytes are a function of the keys and the items only.
    ///
    /// Three passes over the source's L1 pages (L0, L1, data), none through the caches; memory is L0
    /// plus one L1 page plus one data page. `out` is neither flushed nor shut down.
    pub async fn write_compacted_to<TOut: AsyncWrite + Unpin>(
        &self,
        out: &mut TOut,
    ) -> Result<u64, ArchiveError> {
        let key_len = l1_page_len::<TPrimaryKey>(COMPACT_L1_CAPACITY);
        let data_len = data_page_len::<TSlotKey, TItem>() as u64;

        let ranges = self.compact_ranges().await?;
        let keys = ranges.iter().map(|(_, _, count)| *count).sum::<usize>();
        let pages = ranges.len();

        let l0_len = l0_frame_len::<TPrimaryKey>(pages) as u64;
        let data_start = l0_len + (pages * key_len) as u64;

        let l0: Vec<L0Entry<TPrimaryKey>> = ranges
            .into_iter()
            .enumerate()
            .map(|(page, (from, to, _))| L0Entry {
                from,
                to,
                offset: l0_len + (page * key_len) as u64,
            })
            .collect();

        let mut written = 0u64;
        let body = encode_l0_bodies(&l0, pages.max(1))?
            .pop()
            .map(|frame| frame.body)
            .unwrap_or_default();
        let frame = encode_l0_frame(L0_HEADER_LEN + body.len(), 0, pages, &body)?;
        out.write_all(&frame).await?;
        written += frame.len() as u64;
        drop(l0);

        let mut page: Vec<(TPrimaryKey, u64)> = Vec::with_capacity(COMPACT_L1_CAPACITY);
        let mut rank = 0u64;

        for interval in &self.l0 {
            for (key, _) in self.source_l1_page(interval).await? {
                page.push((key, data_start + rank * data_len));
                rank += 1;

                if page.len() == COMPACT_L1_CAPACITY {
                    written += write_l1(out, &page).await?;
                    page.clear();
                }
            }
        }

        if !page.is_empty() {
            written += write_l1(out, &page).await?;
        }
        drop(page);

        if rank as usize != keys {
            return Err(changed_underneath());
        }

        for interval in &self.l0 {
            for (_, offset) in self.source_l1_page(interval).await? {
                let bytes = self.read_data_bytes(offset).await?;
                out.write_all(&bytes).await?;
                written += bytes.len() as u64;
            }
        }

        if written != compact_len::<TPrimaryKey, TSlotKey, TItem>(keys) as u64 {
            return Err(changed_underneath());
        }

        Ok(written)
    }

    /// `(from, to, count)` of every compact L1 page, checking that keys ascend across source pages.
    async fn compact_ranges(&self) -> Result<Vec<(TPrimaryKey, TPrimaryKey, usize)>, ArchiveError> {
        let mut ranges: Vec<(TPrimaryKey, TPrimaryKey, usize)> = Vec::new();
        let mut previous: Option<TPrimaryKey> = None;

        for interval in &self.l0 {
            for (key, _) in self.source_l1_page(interval).await? {
                if previous.as_ref().is_some_and(|previous| *previous >= key) {
                    return Err(ArchiveError::Corrupted(
                        "keys do not ascend across L1 pages".to_string(),
                    ));
                }

                match ranges.last_mut() {
                    Some((_, to, count)) if *count < COMPACT_L1_CAPACITY => {
                        *to = key.clone();
                        *count += 1;
                    }
                    _ => ranges.push((key.clone(), key.clone(), 1)),
                }

                previous = Some(key);
            }
        }

        Ok(ranges)
    }

    async fn source_l1_page(
        &self,
        interval: &L0Entry<TPrimaryKey>,
    ) -> Result<Vec<(TPrimaryKey, u64)>, ArchiveError> {
        let bytes = self.read_l1_bytes(interval.offset).await?;
        decode_l1_page(&bytes, interval)
    }
}

fn compact_len<TPrimaryKey, TSlotKey, TItem>(keys: usize) -> usize
where
    TPrimaryKey: PrimaryKeyFixedSize,
    TSlotKey: PreallocatedSlotKey,
    TItem: PreallocatedDataItem,
{
    let pages = keys.div_ceil(COMPACT_L1_CAPACITY);

    l0_frame_len::<TPrimaryKey>(pages)
        + pages * l1_page_len::<TPrimaryKey>(COMPACT_L1_CAPACITY)
        + keys * data_page_len::<TSlotKey, TItem>()
}

async fn write_l1<TPrimaryKey: PrimaryKeyFixedSize, TOut: AsyncWrite + Unpin>(
    out: &mut TOut,
    page: &[(TPrimaryKey, u64)],
) -> Result<u64, ArchiveError> {
    let bytes = encode_l1_page(page, COMPACT_L1_CAPACITY)?;
    out.write_all(&bytes).await?;
    Ok(bytes.len() as u64)
}

fn changed_underneath() -> ArchiveError {
    ArchiveError::Corrupted("the source changed while it was being compacted".to_string())
}
