use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(test)]
use crate::FinishCounters;
use crate::{
    IoReaderFactory, IoWriterFactory, LocalFileReader, LocalFileWriter, data_file_name,
    file_io_error, index_file_name,
};

/// The files of ONE archive in a local directory — the index as [`index_file_name`], data file `n` as
/// [`data_file_name`]`(n)` — through both factories, so a working writer that reads its own pages back is
/// built over it as well as a reader or a compaction's output.
///
/// * `create_*` makes the directory when it is missing, removes a file already there under the name, and
///   starts a NEW empty file: on unix a reader opened on the old file keeps reading the old bytes, as over
///   the in-memory factory, instead of finding them cut under it. Only the files created are replaced — a
///   `dataXX` an earlier, larger archive left in the directory stays, named by no index, so what belongs
///   to an archive is what its plan or its writer says, never a listing of the directory.
/// * `create_*_reader` opens a file that exists; a missing file — or directory — is `NotFound`, never an
///   empty reader.
///
/// A clone is the same directory. Nothing is read or written until a file is asked for.
#[derive(Debug, Clone)]
pub struct LocalFileIo {
    dir: Arc<Path>,
    #[cfg(test)]
    counters: Arc<FinishCounters>,
}

impl LocalFileIo {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Arc::from(dir.into()),
            #[cfg(test)]
            counters: Arc::default(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join(index_file_name())
    }

    pub fn data_path(&self, no: u8) -> PathBuf {
        self.dir.join(data_file_name(no))
    }

    #[cfg(test)]
    pub(crate) fn counters(&self) -> &FinishCounters {
        &self.counters
    }

    async fn create(&self, path: PathBuf) -> std::io::Result<LocalFileWriter> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| file_io_error(error, "create the directory", &self.dir))?;

        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(file_io_error(error, "remove the old file", &path)),
        }

        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|error| file_io_error(error, "create", &path))?
            .into_std()
            .await;

        #[cfg(not(test))]
        let writer = LocalFileWriter::new(file, path);
        #[cfg(test)]
        let writer = LocalFileWriter::new(file, path, self.counters.clone());

        Ok(writer)
    }

    async fn open(&self, path: PathBuf) -> std::io::Result<LocalFileReader> {
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|error| file_io_error(error, "open", &path))?
            .into_std()
            .await;

        Ok(LocalFileReader::new(file, path))
    }
}

impl IoWriterFactory<LocalFileWriter> for LocalFileIo {
    async fn create_index_writer(&self) -> std::io::Result<LocalFileWriter> {
        self.create(self.index_path()).await
    }

    async fn create_data_writer(&self, no: u8) -> std::io::Result<LocalFileWriter> {
        self.create(self.data_path(no)).await
    }
}

impl IoReaderFactory<LocalFileReader> for LocalFileIo {
    async fn create_index_reader(&self) -> std::io::Result<LocalFileReader> {
        self.open(self.index_path()).await
    }

    async fn create_data_reader(&self, no: u8) -> std::io::Result<LocalFileReader> {
        self.open(self.data_path(no)).await
    }
}
