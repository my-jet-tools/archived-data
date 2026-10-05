use std::fs::File;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{ArchiveFileWriter, file_io_error, pwrite_all, run_blocking_file_op};

/// One local file of an archive being written, as [`ArchiveFileWriter`] prescribes: `append` at the end,
/// `write_at` inside what is written, `finish` = `fsync` (`sync_all`), and `Drop` finishing a file nobody
/// finished.
///
/// Every byte of a call reaches the file before the call returns, so a reader of the same file sees it at
/// once. `finish` syncs the file's content; it does not sync the directory entry of a new file, so a crash of
/// the machine can lose a file whose `finish` returned — an archive written here is a working copy, rebuilt
/// by whoever made it. A failed `append` cuts the file back to where it started; a failed `write_at` can
/// leave its range partly written. Neither call is cancel-safe: dropped halfway, its bytes may still land.
pub struct LocalFileWriter {
    // A tokio mutex because the guard is held across the file IO: appends take their offsets in the order
    // they were called, and a failed one cuts its bytes off before the next one starts. A parking_lot guard
    // cannot cross an await.
    inner: Arc<tokio::sync::Mutex<Option<LocalFileWriterInner>>>,
    path: Arc<Path>,
}

struct LocalFileWriterInner {
    file: Arc<File>,
    path: Arc<Path>,
    len: u64,
    #[cfg(test)]
    counters: Arc<FinishCounters>,
}

impl LocalFileWriterInner {
    fn sync(&self) -> std::io::Result<()> {
        self.file
            .sync_all()
            .map_err(|error| file_io_error(error, "sync", &self.path))?;

        #[cfg(test)]
        self.counters.finishes.fetch_add(1, Ordering::AcqRel);

        Ok(())
    }
}

/// What the writers of one factory did about finishing, so a test can see `finish` and `Drop` keep their
/// contract.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FinishCounters {
    finishes: AtomicUsize,
    finishes_spawned_by_drop: AtomicUsize,
}

#[cfg(test)]
impl FinishCounters {
    /// Every sync that succeeded, whether `finish` or `Drop` ran it.
    pub fn finishes(&self) -> usize {
        self.finishes.load(Ordering::Acquire)
    }

    pub fn finishes_spawned_by_drop(&self) -> usize {
        self.finishes_spawned_by_drop.load(Ordering::Acquire)
    }
}

impl LocalFileWriter {
    pub(crate) fn new(
        file: File,
        path: PathBuf,
        #[cfg(test)] counters: Arc<FinishCounters>,
    ) -> Self {
        let path: Arc<Path> = Arc::from(path);

        let inner = LocalFileWriterInner {
            file: Arc::new(file),
            path: path.clone(),
            len: 0,
            #[cfg(test)]
            counters,
        };

        Self {
            inner: Arc::new(tokio::sync::Mutex::new(Some(inner))),
            path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn finished(&self) -> ! {
        panic!(
            "the archive file {} is already finished: nothing can be written to it",
            self.path.display()
        )
    }
}

impl ArchiveFileWriter for LocalFileWriter {
    async fn append(&self, bytes: &[u8]) -> std::io::Result<u64> {
        let mut slot = self.inner.lock().await;
        let inner = slot.as_mut().unwrap_or_else(|| self.finished());
        let offset = inner.len;

        if bytes.is_empty() {
            return Ok(offset);
        }

        let end = offset + bytes.len() as u64;
        let (file, path, bytes) = (inner.file.clone(), inner.path.clone(), bytes.to_vec());

        run_blocking_file_op(move || append_at(&file, &bytes, offset, &path)).await?;

        inner.len = end;
        Ok(offset)
    }

    async fn write_at(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let slot = self.inner.lock().await;
        let inner = slot.as_ref().unwrap_or_else(|| self.finished());

        let fits = offset
            .checked_add(bytes.len() as u64)
            .is_some_and(|end| end <= inner.len);

        if !fits {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{} bytes at {offset} reach past the end of the {}-byte file {}",
                    bytes.len(),
                    inner.len,
                    inner.path.display()
                ),
            ));
        }

        if bytes.is_empty() {
            return Ok(());
        }

        let (file, path, bytes) = (inner.file.clone(), inner.path.clone(), bytes.to_vec());

        run_blocking_file_op(move || {
            pwrite_all(&file, &bytes, offset).map_err(|error| {
                let doing = format!("write {} bytes at {offset} of", bytes.len());
                file_io_error(error, &doing, &path)
            })
        })
        .await
    }

    async fn size(&self) -> std::io::Result<u64> {
        let slot = self.inner.lock().await;
        Ok(slot.as_ref().unwrap_or_else(|| self.finished()).len)
    }

    fn finish(&self) -> impl Future<Output = std::io::Result<()>> + Send + 'static {
        let inner = self.inner.clone();

        async move {
            let taken = inner.lock().await.take();

            match taken {
                Some(inner) => run_blocking_file_op(move || inner.sync()).await,
                None => Ok(()),
            }
        }
    }
}

impl Drop for LocalFileWriter {
    fn drop(&mut self) {
        // Held only by a `finish` in flight, which has taken the file already.
        let Ok(mut slot) = self.inner.try_lock() else {
            return;
        };

        let Some(inner) = slot.take() else {
            return;
        };

        #[cfg(test)]
        inner
            .counters
            .finishes_spawned_by_drop
            .fetch_add(1, Ordering::AcqRel);

        // Nobody is left to hand a failed sync to, so it panics where it runs, naming the file.
        let sync = move || {
            if let Err(error) = inner.sync() {
                panic!("the finish of an archive file dropped unfinished failed: {error}");
            }
        };

        // A writer can outlive the runtime it was made in; the sync then runs on a thread of its own.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(sync);
            }
            Err(_) => {
                std::thread::spawn(sync);
            }
        }
    }
}

/// `bytes` at `offset`, the end of the file; on a failure the file is cut back to `offset`, so a failed
/// append leaves no bytes past the end the writer counts.
fn append_at(file: &File, bytes: &[u8], offset: u64, path: &Path) -> std::io::Result<()> {
    let Err(error) = pwrite_all(file, bytes, offset) else {
        return Ok(());
    };

    let doing = format!("append {} bytes at {offset} to", bytes.len());
    let error = file_io_error(error, &doing, path);

    match file.set_len(offset) {
        Ok(()) => Err(error),
        Err(cut) => Err(std::io::Error::new(
            error.kind(),
            format!("{error}; cutting the file back to {offset} bytes failed too: {cut}"),
        )),
    }
}
