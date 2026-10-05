use std::future::Future;

/// One file of an archive being written, with random access: bytes are appended at the end, and bytes
/// already written are overwritten in place.
///
/// The archive knows neither what the file is called nor whether it is a file at all — that is the
/// factory's and this implementation's business. An S3 object, for one, cannot be written in the
/// middle: its implementation keeps the bytes where it can, and sends them on `finish`.
///
/// `finish` is where the file is confirmed, and an `Err` from it means the file is not there. It confirms
/// THIS file, and only that what was written to it is persisted — never that the archive it belongs to is
/// complete. `Drop` means the same: a writer dropped before `finish` still persists what reached it, so an
/// archive abandoned halfway leaves its files behind.
///
/// What says an archive is complete depends on its shape, and a file's `finish` is never it:
///
/// * a COMPACT archive is complete when its index EXISTS AND OPENS. A compaction finishes every data file
///   first and the index last, and a reader refuses as `Corrupted` an index that ends before or inside an
///   L1 page its L0 names — which is what an index pass that failed, or was dropped, leaves;
/// * a WORKING archive's index exists from the moment its writer is created, so its existing says nothing.
///   It opens only when its last `flush` COMPLETED and nothing was written to its files after it; otherwise
///   `open` is `Corrupted`. Frame 0's hash does not verify until the first flush completes, the first byte
///   written to the index or a data file after a completed flush is preceded by a write that spoils it
///   again, and a flush writes frame 0 last. Everything written is in it only once the writer's `finish`
///   returned `Ok`.
///
/// The shape of an implementation: everything lives in an `Inner` held as
/// `Arc<tokio::sync::Mutex<Option<Inner>>>` — the lock costs microseconds next to the I/O it guards.
///
/// * `finish` clones the `Arc` into its future, which locks, takes the `Inner` out and finishes it.
///   Called again, it finds nothing and returns `Ok(())`.
/// * `Drop` tries to take the `Inner` too, with `try_lock`. Only if it is still there — the file was
///   never finished — does it spawn the finish; otherwise it does nothing at all.
/// * Every other method reaches into the `Inner` and PANICS when it is gone: writing to a finished file
///   is a bug in the caller, not a state to recover from.
///
/// `finish`'s future is `'static` because `Drop` hands it to `tokio::spawn`.
pub trait ArchiveFileWriter: Send + Sync {
    /// Writes `bytes` at the end of the file and returns the offset they start at.
    fn append(&self, bytes: &[u8]) -> impl Future<Output = std::io::Result<u64>> + Send;

    /// Overwrites bytes already written. A range reaching past the end of the file is an `InvalidInput`
    /// error: a file grows only by `append`.
    fn write_at(
        &self,
        offset: u64,
        bytes: &[u8],
    ) -> impl Future<Output = std::io::Result<()>> + Send;

    /// How many bytes the file holds.
    fn size(&self) -> impl Future<Output = std::io::Result<u64>> + Send;

    fn finish(&self) -> impl Future<Output = std::io::Result<()>> + Send + 'static;
}
