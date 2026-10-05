use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::{ArchiveFileReader, ArchiveFileWriter, IoReaderFactory, IoWriterFactory};

/// Which file of an archive: the factories are asked by role, never by name. Ordered as the index, then
/// the data files by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileRole {
    Index,
    Data(u8),
}

/// One file held in memory: its bytes, how it was finished, and counters of what was done to it, so a
/// test can observe the IO an archive performs. It can also be told to fail, or to hold reads back.
#[derive(Default)]
pub struct InMemoryFile {
    role: Option<FileRole>,
    bytes: tokio::sync::Mutex<Vec<u8>>,
    finishes: AtomicUsize,
    finishes_spawned_by_drop: AtomicUsize,
    finish_attempts: AtomicUsize,
    appends: AtomicUsize,
    write_ats: AtomicUsize,
    write_at_ranges: Mutex<Vec<(u64, usize)>>,
    read_ats: AtomicUsize,
    bytes_read: AtomicUsize,
    largest_read: AtomicUsize,
    eof_reads: AtomicUsize,
    readers_opened: AtomicUsize,
    fail_reads: FailFrom,
    fail_appends: FailFrom,
    fail_write_ats: FailFrom,
    fail_finish: AtomicBool,
    gated_offset: Mutex<Option<u64>>,
    gate: Notify,
    waiting: AtomicBool,
    writes_shut: AtomicBool,
    writes_shut_from: Mutex<Option<usize>>,
    write_waiting: AtomicBool,
}

/// The number of the first call of one kind that fails — calls are numbered from 0 as the file counts
/// them — or none.
#[derive(Default)]
struct FailFrom(Mutex<Option<usize>>);

impl FailFrom {
    fn set(&self, calls_so_far: &AtomicUsize, succeeding: usize) {
        *self.0.lock().unwrap() = Some(calls_so_far.load(Ordering::Acquire) + succeeding);
    }

    fn fails(&self, call: usize) -> bool {
        self.0.lock().unwrap().is_some_and(|from| call >= from)
    }
}

impl InMemoryFile {
    fn of_role(role: FileRole, bytes: Vec<u8>) -> Self {
        Self {
            role: Some(role),
            bytes: tokio::sync::Mutex::new(bytes),
            ..Default::default()
        }
    }

    pub async fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().await.clone()
    }

    pub fn finishes(&self) -> usize {
        self.finishes.load(Ordering::Acquire)
    }

    pub fn finishes_spawned_by_drop(&self) -> usize {
        self.finishes_spawned_by_drop.load(Ordering::Acquire)
    }

    /// Every finish that took the file, whether it succeeded or failed.
    pub fn finish_attempts(&self) -> usize {
        self.finish_attempts.load(Ordering::Acquire)
    }

    /// Every `append` called, whatever it returned.
    pub fn appends(&self) -> usize {
        self.appends.load(Ordering::Acquire)
    }

    /// Every `write_at` called, whatever it returned.
    pub fn write_ats(&self) -> usize {
        self.write_ats.load(Ordering::Acquire)
    }

    /// Every `write_at` of exactly `len` bytes at `offset` called, whatever it returned.
    pub fn write_ats_of(&self, offset: u64, len: usize) -> usize {
        self.write_at_ranges
            .lock()
            .unwrap()
            .iter()
            .filter(|range| **range == (offset, len))
            .count()
    }

    /// Every `read_at` called, whatever it returned.
    pub fn read_ats(&self) -> usize {
        self.read_ats.load(Ordering::Acquire)
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes_read.load(Ordering::Acquire)
    }

    /// The most bytes one `read_at` asked for.
    pub fn largest_read(&self) -> usize {
        self.largest_read.load(Ordering::Acquire)
    }

    /// The `read_at`s that reached past the end of the file.
    pub fn eof_reads(&self) -> usize {
        self.eof_reads.load(Ordering::Acquire)
    }

    pub fn readers_opened(&self) -> usize {
        self.readers_opened.load(Ordering::Acquire)
    }

    /// Every later `read_at` at exactly `offset` parks until [`Self::open_reads`].
    pub fn shut_reads_at(&self, offset: u64) {
        *self.gated_offset.lock().unwrap() = Some(offset);
    }

    pub fn open_reads(&self) {
        *self.gated_offset.lock().unwrap() = None;
        self.gate.notify_waiters();
    }

    pub fn is_read_waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    /// Every later `append` and `write_at` parks until [`Self::open_writes`], before it changes anything.
    pub fn shut_writes(&self) {
        self.writes_shut.store(true, Ordering::Release);
    }

    /// The next `succeeding` `append`s and `write_at`s — counted together, in the order they are called —
    /// go through, and every later one parks until [`Self::open_writes`], before it changes anything. So a
    /// test can stop a call at any one of its writes, not only at its first.
    pub fn shut_writes_after(&self, succeeding: usize) {
        *self.writes_shut_from.lock().unwrap() = Some(self.writes() + succeeding);
    }

    pub fn open_writes(&self) {
        self.writes_shut.store(false, Ordering::Release);
        *self.writes_shut_from.lock().unwrap() = None;
        self.gate.notify_waiters();
    }

    /// Every `append` and `write_at` called, whatever it returned.
    fn writes(&self) -> usize {
        self.appends() + self.write_ats()
    }

    pub fn is_write_waiting(&self) -> bool {
        self.write_waiting.load(Ordering::Acquire)
    }

    /// Every later `read_at` fails.
    pub fn fail_reads(&self) {
        self.fail_reads_after(0);
    }

    /// The next `succeeding` `read_at`s go through, and every one after them fails.
    pub fn fail_reads_after(&self, succeeding: usize) {
        self.fail_reads.set(&self.read_ats, succeeding);
    }

    /// Every later `append` fails and leaves the file as it was.
    pub fn fail_appends(&self) {
        self.fail_appends_after(0);
    }

    /// The next `succeeding` `append`s go through, and every one after them fails.
    pub fn fail_appends_after(&self, succeeding: usize) {
        self.fail_appends.set(&self.appends, succeeding);
    }

    /// Every later `write_at` fails and leaves the file as it was.
    pub fn fail_write_ats(&self) {
        self.fail_write_ats_after(0);
    }

    /// The next `succeeding` `write_at`s go through, and every one after them fails.
    pub fn fail_write_ats_after(&self, succeeding: usize) {
        self.fail_write_ats.set(&self.write_ats, succeeding);
    }

    /// Every later finish takes the file and fails, with an error that names the file's role.
    pub fn fail_finish(&self) {
        self.fail_finish.store(true, Ordering::Release);
    }

    async fn finish_now(&self) -> std::io::Result<()> {
        tokio::task::yield_now().await;
        self.finish_attempts.fetch_add(1, Ordering::AcqRel);

        if self.fail_finish.load(Ordering::Acquire) {
            let role = self
                .role
                .map_or("unnamed".to_string(), |role| format!("{role:?}"));
            return Err(std::io::Error::other(format!(
                "the {role} file is set to fail its finish"
            )));
        }

        self.finishes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn pass_gate(&self, offset: u64) {
        let mut waited = false;

        loop {
            // Registered before the check, so an `open_reads` between the two is not missed.
            let opened = self.gate.notified();
            let gated = *self.gated_offset.lock().unwrap() == Some(offset);

            if !gated {
                break;
            }

            waited = true;
            self.waiting.store(true, Ordering::Release);
            opened.await;
        }

        if waited {
            self.waiting.store(false, Ordering::Release);
        }
    }

    /// `write` is the number of this write among every `append` and `write_at`, counted from 0.
    async fn pass_write_gate(&self, write: usize) {
        let mut waited = false;

        loop {
            let opened = self.gate.notified();

            let shut = self.writes_shut.load(Ordering::Acquire)
                || self
                    .writes_shut_from
                    .lock()
                    .unwrap()
                    .is_some_and(|from| write >= from);

            if !shut {
                break;
            }

            waited = true;
            self.write_waiting.store(true, Ordering::Release);
            opened.await;
        }

        if waited {
            self.write_waiting.store(false, Ordering::Release);
        }
    }
}

/// Every file of one archive, in memory: the test implementation of both factories. A reader sees every
/// byte the writer of the same file has written so far. A clone shares every file, so a test keeps a
/// handle to the files after handing the factory away.
#[derive(Clone, Default)]
pub struct InMemoryIo {
    files: Arc<Mutex<HashMap<FileRole, Arc<InMemoryFile>>>>,
    prepared: Arc<Mutex<HashMap<FileRole, Arc<InMemoryFile>>>>,
    created: Arc<Mutex<Vec<FileRole>>>,
}

impl InMemoryIo {
    pub fn file(&self, role: FileRole) -> Option<Arc<InMemoryFile>> {
        self.files.lock().unwrap().get(&role).cloned()
    }

    /// A file put in place as it is — bytes no writer would produce. It is not a creation.
    pub fn put(&self, role: FileRole, bytes: Vec<u8>) -> Arc<InMemoryFile> {
        let file = Arc::new(InMemoryFile::of_role(role, bytes));
        self.files.lock().unwrap().insert(role, file.clone());
        file
    }

    /// Every file there is, in role order.
    pub fn roles(&self) -> Vec<FileRole> {
        let mut roles: Vec<FileRole> = self.files.lock().unwrap().keys().copied().collect();
        roles.sort();
        roles
    }

    /// Every writer created, in creation order.
    pub fn created_roles(&self) -> Vec<FileRole> {
        self.created.lock().unwrap().clone()
    }

    /// The empty file the NEXT writer created for `role` will write into, handed out now so a test can tell
    /// it to fail before the archive creates it. It is not a file of the archive until then.
    pub fn prepare(&self, role: FileRole) -> Arc<InMemoryFile> {
        let file = Arc::new(InMemoryFile::of_role(role, Vec::new()));
        self.prepared.lock().unwrap().insert(role, file.clone());
        file
    }

    fn create(&self, role: FileRole) -> InMemoryWriter {
        let prepared = self.prepared.lock().unwrap().remove(&role);
        let file = prepared.unwrap_or_else(|| Arc::new(InMemoryFile::of_role(role, Vec::new())));
        self.files.lock().unwrap().insert(role, file.clone());
        self.created.lock().unwrap().push(role);

        InMemoryWriter {
            inner: Arc::new(tokio::sync::Mutex::new(Some(file))),
        }
    }

    async fn open(&self, role: FileRole) -> std::io::Result<InMemoryReader> {
        // A real factory is a request: give concurrent openers the chance to race.
        tokio::task::yield_now().await;

        let file = self.file(role).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("no {role:?} file"))
        })?;

        file.readers_opened.fetch_add(1, Ordering::AcqRel);
        Ok(InMemoryReader { file })
    }
}

impl IoWriterFactory<InMemoryWriter> for InMemoryIo {
    async fn create_index_writer(&self) -> std::io::Result<InMemoryWriter> {
        Ok(self.create(FileRole::Index))
    }

    async fn create_data_writer(&self, no: u8) -> std::io::Result<InMemoryWriter> {
        Ok(self.create(FileRole::Data(no)))
    }
}

impl IoReaderFactory<InMemoryReader> for InMemoryIo {
    async fn create_index_reader(&self) -> std::io::Result<InMemoryReader> {
        self.open(FileRole::Index).await
    }

    async fn create_data_reader(&self, no: u8) -> std::io::Result<InMemoryReader> {
        self.open(FileRole::Data(no)).await
    }
}

pub struct InMemoryWriter {
    inner: Arc<tokio::sync::Mutex<Option<Arc<InMemoryFile>>>>,
}

impl InMemoryWriter {
    async fn file(&self) -> Arc<InMemoryFile> {
        self.inner
            .lock()
            .await
            .clone()
            .expect("the archive file is already finished: nothing can be written to it")
    }
}

impl ArchiveFileWriter for InMemoryWriter {
    async fn append(&self, bytes: &[u8]) -> std::io::Result<u64> {
        let file = self.file().await;
        let call = file.appends.fetch_add(1, Ordering::AcqRel);
        file.pass_write_gate(file.writes() - 1).await;

        if file.fail_appends.fails(call) {
            return Err(std::io::Error::other(
                "appends to this file are set to fail",
            ));
        }

        let mut content = file.bytes.lock().await;

        let offset = content.len() as u64;
        content.extend_from_slice(bytes);
        Ok(offset)
    }

    async fn write_at(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let file = self.file().await;
        let call = file.write_ats.fetch_add(1, Ordering::AcqRel);
        file.write_at_ranges
            .lock()
            .unwrap()
            .push((offset, bytes.len()));
        file.pass_write_gate(file.writes() - 1).await;

        if file.fail_write_ats.fails(call) {
            return Err(std::io::Error::other(
                "write_ats to this file are set to fail",
            ));
        }

        let mut content = file.bytes.lock().await;

        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(bytes.len())?))
            .filter(|range| range.end <= content.len());

        let Some(range) = range else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{} bytes at {offset} reach past the end of a {}-byte file",
                    bytes.len(),
                    content.len()
                ),
            ));
        };

        content[range].copy_from_slice(bytes);
        Ok(())
    }

    async fn size(&self) -> std::io::Result<u64> {
        let file = self.file().await;
        let content = file.bytes.lock().await;
        Ok(content.len() as u64)
    }

    fn finish(&self) -> impl Future<Output = std::io::Result<()>> + Send + 'static {
        let inner = self.inner.clone();

        async move {
            let taken = inner.lock().await.take();

            match taken {
                Some(file) => file.finish_now().await,
                None => Ok(()),
            }
        }
    }
}

impl Drop for InMemoryWriter {
    fn drop(&mut self) {
        // Held only by a `finish` in flight, which has taken the file already.
        let Ok(mut slot) = self.inner.try_lock() else {
            return;
        };

        let Some(file) = slot.take() else {
            return;
        };

        file.finishes_spawned_by_drop.fetch_add(1, Ordering::AcqRel);

        tokio::spawn(async move {
            // Nobody is left to hand a failed finish to; `finish_attempts` still counts it.
            let _ = file.finish_now().await;
        });
    }
}

pub struct InMemoryReader {
    file: Arc<InMemoryFile>,
}

impl ArchiveFileReader for InMemoryReader {
    async fn read_at(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let call = self.file.read_ats.fetch_add(1, Ordering::AcqRel);
        self.file.largest_read.fetch_max(len, Ordering::AcqRel);
        self.file.pass_gate(offset).await;

        if self.file.fail_reads.fails(call) {
            return Err(std::io::Error::other("reads of this file are set to fail"));
        }

        let content = self.file.bytes.lock().await;

        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(len)?))
            .filter(|range| range.end <= content.len());

        match range {
            Some(range) => {
                self.file.bytes_read.fetch_add(len, Ordering::AcqRel);
                Ok(content[range].to_vec())
            }
            None => {
                self.file.eof_reads.fetch_add(1, Ordering::AcqRel);
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "{len} bytes at {offset} reach past the end of a {}-byte file",
                        content.len()
                    ),
                ))
            }
        }
    }

    async fn size(&self) -> std::io::Result<u64> {
        Ok(self.file.bytes.lock().await.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// What an archive does with its files, written generically over the traits, so the test also proves
    /// every future involved is `Send`.
    async fn write_then_read_back<TFactory, TWriter, TReader>(io: &TFactory) -> (u64, u64, Vec<u8>)
    where
        TFactory: IoWriterFactory<TWriter> + IoReaderFactory<TReader>,
        TWriter: ArchiveFileWriter,
        TReader: ArchiveFileReader,
    {
        let writer = io.create_data_writer(7).await.unwrap();
        let first = writer.append(b"hello").await.unwrap();
        let second = writer.append(b" world").await.unwrap();
        writer.write_at(0, b"J").await.unwrap();

        let reader = io.create_data_reader(7).await.unwrap();
        let read = reader.read_at(1, 4).await.unwrap();
        assert_eq!(reader.size().await.unwrap(), 11);
        assert_eq!(writer.size().await.unwrap(), 11);

        writer.finish().await.unwrap();
        (first, second, read)
    }

    async fn wait_until(what: &str, condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} never happened"));
    }

    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_writer_appends_and_overwrites_and_a_reader_of_the_same_file_sees_it_all() {
        let io = Arc::new(InMemoryIo::default());
        let in_task = io.clone();

        let (first, second, read) = tokio::spawn(async move {
            write_then_read_back::<InMemoryIo, InMemoryWriter, InMemoryReader>(&in_task).await
        })
        .await
        .unwrap();

        assert_eq!((first, second), (0, 5));
        assert_eq!(read, b"ello");

        settle().await;
        let file = io.file(FileRole::Data(7)).unwrap();
        assert_eq!(file.bytes().await, b"Jello world");
        assert_eq!(file.finishes(), 1);
        assert_eq!(file.finishes_spawned_by_drop(), 0);
    }

    #[tokio::test]
    async fn ranges_past_the_end_and_missing_files_are_errors() {
        let io = InMemoryIo::default();

        let writer = io.create_index_writer().await.unwrap();
        writer.append(b"abcd").await.unwrap();

        let past_end = writer.write_at(3, b"xy").await.unwrap_err();
        assert_eq!(past_end.kind(), std::io::ErrorKind::InvalidInput);

        let reader = io.create_index_reader().await.unwrap();
        let short = reader.read_at(3, 2).await.unwrap_err();
        assert_eq!(short.kind(), std::io::ErrorKind::UnexpectedEof);

        let missing = io.create_data_reader(9).await.map(|_| ()).unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

        writer.finish().await.unwrap();
    }

    #[tokio::test]
    async fn creating_a_writer_starts_an_empty_file() {
        let io = InMemoryIo::default();

        let first = io.create_data_writer(0).await.unwrap();
        first.append(b"old").await.unwrap();
        first.finish().await.unwrap();

        let second = io.create_data_writer(0).await.unwrap();
        assert_eq!(second.size().await.unwrap(), 0);
        second.finish().await.unwrap();
    }

    #[tokio::test]
    async fn a_second_finish_does_nothing_and_the_drop_after_it_spawns_nothing() {
        let io = InMemoryIo::default();

        let writer = io.create_data_writer(1).await.unwrap();
        writer.append(b"x").await.unwrap();
        writer.finish().await.unwrap();
        writer.finish().await.unwrap();
        drop(writer);

        settle().await;
        let file = io.file(FileRole::Data(1)).unwrap();
        assert_eq!(file.finishes(), 1);
        assert_eq!(file.finishes_spawned_by_drop(), 0);
    }

    #[tokio::test]
    async fn a_writer_dropped_without_finish_is_finished_by_drop() {
        let io = InMemoryIo::default();

        let writer = io.create_data_writer(2).await.unwrap();
        writer.append(b"abc").await.unwrap();
        drop(writer);

        let file = io.file(FileRole::Data(2)).unwrap();
        wait_until("the finish spawned by drop", || file.finishes() == 1).await;
        settle().await;
        assert_eq!(file.finishes(), 1);
        assert_eq!(file.finishes_spawned_by_drop(), 1);
    }

    #[tokio::test]
    async fn a_finish_awaited_after_the_drop_still_finishes_exactly_once() {
        let io = InMemoryIo::default();

        let writer = io.create_data_writer(3).await.unwrap();
        writer.append(b"zz").await.unwrap();

        let finishing = writer.finish();
        drop(writer);
        finishing.await.unwrap();

        let file = io.file(FileRole::Data(3)).unwrap();
        wait_until("the finish", || file.finishes() == 1).await;
        settle().await;
        assert_eq!(file.finishes(), 1);
    }

    #[tokio::test]
    #[should_panic(expected = "already finished")]
    async fn writing_to_a_finished_file_panics() {
        let io = InMemoryIo::default();

        let writer = io.create_data_writer(4).await.unwrap();
        writer.finish().await.unwrap();
        let _ = writer.append(b"late").await;
    }

    #[tokio::test]
    async fn a_clone_shares_every_file_and_the_record_of_creations() {
        let io = InMemoryIo::default();
        let handle = io.clone();

        let writer = io.create_data_writer(2).await.unwrap();
        writer.append(b"shared").await.unwrap();
        io.create_index_writer()
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        handle.put(FileRole::Data(0), b"put".to_vec());

        let file = handle.file(FileRole::Data(2)).unwrap();
        assert_eq!(file.bytes().await, b"shared");
        assert_eq!(
            io.roles(),
            [FileRole::Index, FileRole::Data(0), FileRole::Data(2)]
        );
        assert_eq!(handle.created_roles(), [FileRole::Data(2), FileRole::Index]);

        writer.finish().await.unwrap();
    }

    #[tokio::test]
    async fn every_call_is_counted() {
        let io = InMemoryIo::default();

        let writer = io.create_index_writer().await.unwrap();
        writer.append(b"abcdef").await.unwrap();
        writer.append(b"gh").await.unwrap();
        writer.write_at(1, b"B").await.unwrap();

        let reader = io.create_index_reader().await.unwrap();
        io.create_index_reader().await.unwrap();
        reader.read_at(2, 3).await.unwrap();
        reader.read_at(7, 5).await.unwrap_err();

        let file = io.file(FileRole::Index).unwrap();
        assert_eq!((file.appends(), file.write_ats()), (2, 1));
        assert_eq!((file.write_ats_of(1, 1), file.write_ats_of(1, 2)), (1, 0));
        assert_eq!(
            (file.read_ats(), file.bytes_read(), file.eof_reads()),
            (2, 3, 1)
        );
        assert_eq!(file.largest_read(), 5);
        assert_eq!(file.readers_opened(), 2);

        writer.finish().await.unwrap();
        assert_eq!((file.finish_attempts(), file.finishes()), (1, 1));
    }

    #[tokio::test]
    async fn a_read_at_a_shut_offset_parks_until_reads_open() {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Data(1), (0..10).collect());
        file.shut_reads_at(4);

        let parked = {
            let reader = io.create_data_reader(1).await.unwrap();
            tokio::spawn(async move { reader.read_at(4, 2).await.unwrap() })
        };

        wait_until("the read parking", || file.is_read_waiting()).await;

        let other = io.create_data_reader(1).await.unwrap();
        assert_eq!(other.read_at(3, 2).await.unwrap(), [3, 4]);
        assert!(file.is_read_waiting());

        file.open_reads();
        assert_eq!(parked.await.unwrap(), [4, 5]);
        assert!(!file.is_read_waiting());
    }

    #[tokio::test]
    async fn shut_writes_park_until_writes_open_and_change_nothing_meanwhile() {
        let io = InMemoryIo::default();
        let writer = Arc::new(io.create_index_writer().await.unwrap());
        writer.append(b"ab").await.unwrap();
        let file = io.file(FileRole::Index).unwrap();
        file.shut_writes();

        let parked = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer.write_at(0, b"A").await.unwrap();
                writer.append(b"c").await.unwrap()
            }
        });

        wait_until("the write parking", || file.is_write_waiting()).await;
        assert_eq!(file.bytes().await, b"ab");

        file.open_writes();
        assert_eq!(parked.await.unwrap(), 2);
        assert!(!file.is_write_waiting());
        assert_eq!(file.bytes().await, b"Abc");
    }

    #[tokio::test]
    async fn writes_shut_after_some_go_through_park_at_the_next_and_change_nothing_meanwhile() {
        let io = InMemoryIo::default();
        let writer = Arc::new(io.create_index_writer().await.unwrap());
        writer.append(b"ab").await.unwrap();
        let file = io.file(FileRole::Index).unwrap();

        // Appends and write_ats count together: the write_at and the first append go through.
        file.shut_writes_after(2);

        let parked = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer.write_at(0, b"A").await.unwrap();
                writer.append(b"c").await.unwrap();
                writer.append(b"d").await.unwrap()
            }
        });

        wait_until("the third write parking", || file.is_write_waiting()).await;
        assert_eq!(file.bytes().await, b"Abc");

        file.open_writes();
        assert_eq!(parked.await.unwrap(), 3);
        assert!(!file.is_write_waiting());
        assert_eq!(file.bytes().await, b"Abcd");

        // Opened, the threshold is gone.
        writer.append(b"e").await.unwrap();
        assert_eq!(file.bytes().await, b"Abcde");
    }

    #[tokio::test]
    async fn failing_appends_and_write_ats_are_errors_that_change_nothing() {
        let io = InMemoryIo::default();

        let writer = io.create_index_writer().await.unwrap();
        writer.append(b"abcd").await.unwrap();
        let file = io.file(FileRole::Index).unwrap();

        file.fail_write_ats_after(1);
        writer.write_at(0, b"A").await.unwrap();
        assert!(writer.write_at(1, b"B").await.is_err());
        assert!(writer.write_at(2, b"C").await.is_err());

        file.fail_appends_after(1);
        assert_eq!(writer.append(b"ef").await.unwrap(), 4);
        assert!(writer.append(b"gh").await.is_err());

        assert_eq!(file.bytes().await, b"Abcdef");
        assert_eq!((file.appends(), file.write_ats()), (3, 3));

        file.fail_appends();
        assert!(writer.append(b"ij").await.is_err());
        writer.finish().await.unwrap();
        assert_eq!(file.finishes(), 1);

        let other = io.create_data_writer(0).await.unwrap();
        other.append(b"x").await.unwrap();
        other.write_at(0, b"y").await.unwrap();
        other.finish().await.unwrap();
    }

    #[tokio::test]
    async fn reads_can_fail_after_some_succeed() {
        let io = InMemoryIo::default();
        let file = io.put(FileRole::Data(0), (0..10).collect());
        let reader = io.create_data_reader(0).await.unwrap();

        reader.read_at(0, 1).await.unwrap();
        file.fail_reads_after(2);
        reader.read_at(1, 1).await.unwrap();
        reader.read_at(2, 1).await.unwrap();
        assert!(reader.read_at(3, 1).await.is_err());
        assert_eq!(file.read_ats(), 4);
    }

    #[tokio::test]
    async fn a_prepared_file_is_the_one_the_next_writer_of_its_role_writes() {
        let io = InMemoryIo::default();
        let prepared = io.prepare(FileRole::Data(3));
        prepared.fail_appends_after(1);
        assert!(io.file(FileRole::Data(3)).is_none());
        assert!(io.created_roles().is_empty());

        let writer = io.create_data_writer(3).await.unwrap();
        writer.append(b"first").await.unwrap();
        assert!(writer.append(b"second").await.is_err());
        writer.finish().await.unwrap();

        assert!(Arc::ptr_eq(&io.file(FileRole::Data(3)).unwrap(), &prepared));
        assert_eq!(prepared.bytes().await, b"first");

        let again = io.create_data_writer(3).await.unwrap();
        again.append(b"fresh").await.unwrap();
        again.append(b" file").await.unwrap();
        again.finish().await.unwrap();
        assert!(!Arc::ptr_eq(
            &io.file(FileRole::Data(3)).unwrap(),
            &prepared
        ));
    }

    #[tokio::test]
    async fn failing_reads_and_finishes_are_errors_and_a_failed_finish_is_no_finish() {
        let io = InMemoryIo::default();

        let writer = io.create_data_writer(5).await.unwrap();
        writer.append(b"abc").await.unwrap();

        let file = io.file(FileRole::Data(5)).unwrap();
        let reader = io.create_data_reader(5).await.unwrap();

        file.fail_reads();
        assert!(reader.read_at(0, 1).await.is_err());

        file.fail_finish();
        let failed = writer.finish().await.unwrap_err();
        assert!(failed.to_string().contains("Data(5)"), "{failed}");
        writer.finish().await.unwrap();

        assert_eq!((file.finish_attempts(), file.finishes()), (1, 0));
        drop(writer);
        settle().await;
        assert_eq!(file.finishes_spawned_by_drop(), 0);
    }
}
