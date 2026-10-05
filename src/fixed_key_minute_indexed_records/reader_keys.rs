use crate::{
    ArchiveError, ArchiveFileReader, FixedKeyMinuteIndexedRecordsArchiveReader, IoReaderFactory,
    PrimaryKeyFixedSize, refuse_unordered,
};

/// Every key of a day archive — what a caller that rebuilds the archive from its own records needs, and the
/// one question a reader could not answer before: `get_range` / `get_day` read a key the caller already
/// names, and `keys_count` only counts them.
impl<TKey, TFactory, TReader> FixedKeyMinuteIndexedRecordsArchiveReader<TKey, TFactory, TReader>
where
    TKey: PrimaryKeyFixedSize,
    TFactory: IoReaderFactory<TReader>,
    TReader: ArchiveFileReader,
{
    /// Every key the archive holds, ascending: one read of each L1 page, through none of the caches. Keys
    /// that do not ascend across the pages are `Corrupted`, as a compaction plan refuses them; a failed read
    /// is `Err`, never a shorter list.
    pub async fn keys(&self) -> Result<Vec<TKey>, ArchiveError> {
        let mut keys: Vec<TKey> = Vec::new();
        let mut previous: Option<TKey> = None;

        for interval in &self.l0 {
            for (key, _) in self.source_l1_page(interval).await? {
                refuse_unordered(&previous, &key)?;
                previous = Some(key.clone());
                keys.push(key);
            }
        }

        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{build, compact, open, record, seeded, time};
    use crate::test_items::Login;

    #[tokio::test]
    async fn every_key_of_a_working_and_of_a_compact_archive_is_listed_once_in_order() {
        let mut rng = seeded(7);
        let mut rows = Vec::new();
        let mut expected: Vec<u64> = Vec::new();

        for login in 0..3_000u64 {
            let key = rng.next() % 1_000_000;

            if !expected.contains(&key) {
                expected.push(key);
            }

            rows.push((key, record(time((login % 1_440) as i64, 0), login, 40)));
        }

        expected.sort_unstable();

        let working = open(&build(&rows, 16, 8).await).await;
        let keys: Vec<u64> = working.keys().await.unwrap().iter().map(|k| k.0).collect();
        assert_eq!(keys, expected);

        let compact_io = compact(&working).await;
        let compacted = open(&compact_io).await;
        let keys: Vec<u64> = compacted
            .keys()
            .await
            .unwrap()
            .iter()
            .map(|k| k.0)
            .collect();
        assert_eq!(keys, expected);
        assert_eq!(compacted.keys_count().await.unwrap(), expected.len());
    }

    #[tokio::test]
    async fn an_archive_of_no_key_lists_none() {
        let reader = open(&build(&[], 16, 8).await).await;
        assert!(reader.keys().await.unwrap().is_empty());

        let compacted = open(&compact(&reader).await).await;
        assert_eq!(compacted.keys().await.unwrap(), Vec::<Login>::new());
    }
}
