use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{ArchiveFileReader, file_io_error, pread_exact, run_blocking_file_op};

/// One local file of an archive being read, as [`ArchiveFileReader`] prescribes: `read_at` gives EXACTLY
/// `len` bytes or `UnexpectedEof`, and `size` is what the file holds at the moment of the call — so a reader
/// opened while a writer appends sees the file grow.
///
/// A read holds no position and no lock: each is one positional read on a blocking thread, and concurrent
/// reads of one file run side by side.
pub struct LocalFileReader {
    file: Arc<File>,
    path: Arc<Path>,
}

impl LocalFileReader {
    pub(crate) fn new(file: File, path: PathBuf) -> Self {
        Self {
            file: Arc::new(file),
            path: Arc::from(path),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ArchiveFileReader for LocalFileReader {
    async fn read_at(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let (file, path) = (self.file.clone(), self.path.clone());
        run_blocking_file_op(move || read_exactly(&file, offset, len, &path)).await
    }

    async fn size(&self) -> std::io::Result<u64> {
        let (file, path) = (self.file.clone(), self.path.clone());
        run_blocking_file_op(move || size_of(&file, &path)).await
    }
}

fn size_of(file: &File, path: &Path) -> std::io::Result<u64> {
    file.metadata()
        .map(|metadata| metadata.len())
        .map_err(|error| file_io_error(error, "read the size of", path))
}

/// The size is checked first, so a range past the end allocates nothing and reads nothing; a file cut
/// between the check and the read is `UnexpectedEof` all the same.
fn read_exactly(file: &File, offset: u64, len: usize, path: &Path) -> std::io::Result<Vec<u8>> {
    let size = size_of(file, path)?;

    let fits = offset
        .checked_add(len as u64)
        .is_some_and(|end| end <= size);

    if !fits {
        return Err(past_the_end(offset, len, size, path));
    }

    let mut bytes = vec![0; len];

    if len == 0 {
        return Ok(bytes);
    }

    match pread_exact(file, &mut bytes, offset) {
        Ok(()) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(past_the_end(offset, len, size_of(file, path)?, path))
        }
        Err(error) => {
            let doing = format!("read {len} bytes at {offset} of");
            Err(file_io_error(error, &doing, path))
        }
    }
}

fn past_the_end(offset: u64, len: usize, size: u64, path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!(
            "{len} bytes at {offset} reach past the end of the {size}-byte file {}",
            path.display()
        ),
    )
}
