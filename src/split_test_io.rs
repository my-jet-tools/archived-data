//! What the split tests of every module do with the files of an `InMemoryIo`, whatever the key: read them
//! back, count their reads, park a call inside its IO and drop it there, and recognise a refusal.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use crate::ArchiveError;
use crate::in_memory_io::{FileRole, InMemoryFile, InMemoryIo};

/// What every call on a split writer answers once an earlier write or flush failed or was dropped.
pub const NOT_USABLE: &str =
    "an earlier write or flush failed or was dropped before it completed: the writer is not usable";

/// The message of a `ContractViolation`; anything else fails the test.
pub fn refused<T: std::fmt::Debug>(result: Result<T, ArchiveError>) -> String {
    match result {
        Err(ArchiveError::ContractViolation(message)) => message,
        other => panic!("not refused: {other:?}"),
    }
}

pub async fn file_bytes(io: &InMemoryIo, role: FileRole) -> Vec<u8> {
    io.file(role)
        .unwrap_or_else(|| panic!("no {role:?} file"))
        .bytes()
        .await
}

/// Every data file, in file order; the numbers must run from 0 without a gap.
pub async fn data_files(io: &InMemoryIo) -> Vec<Vec<u8>> {
    let mut files = Vec::new();

    for role in io.roles() {
        if let FileRole::Data(no) = role {
            assert_eq!(
                no as usize,
                files.len(),
                "data files are numbered without a gap"
            );
            files.push(file_bytes(io, role).await);
        }
    }

    files
}

/// Every data file's `read_at`s so far.
pub fn data_read_ats(io: &InMemoryIo) -> usize {
    io.roles()
        .into_iter()
        .filter(|role| matches!(role, FileRole::Data(_)))
        .map(|role| io.file(role).unwrap().read_ats())
        .sum()
}

pub fn data_readers_opened(io: &InMemoryIo) -> usize {
    io.roles()
        .into_iter()
        .filter(|role| matches!(role, FileRole::Data(_)))
        .map(|role| io.file(role).unwrap().readers_opened())
        .sum()
}

pub async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never happened"));
}

/// Polls `call` until `parked` says it is waiting inside its IO, then drops it there.
pub async fn drop_when_parked<T: std::fmt::Debug>(
    call: impl Future<Output = Result<T, ArchiveError>>,
    parked: impl Fn() -> bool,
    case: &str,
) {
    tokio::pin!(call);

    tokio::select! {
        result = &mut call => panic!("{case}: the call never parked: {result:?}"),
        _ = wait_until(case, parked) => {}
    }
}

/// Polls `call` once, on the test's own task: what a call does between two polls is the test's to decide.
pub async fn poll_once<F: Future>(mut call: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx))).await
}

/// Polls `call` until `parked` says it waits where the test wants it; a call that completes first fails the
/// test.
pub async fn poll_until_parked<F: Future>(mut call: Pin<&mut F>, parked: impl Fn() -> bool) {
    for _ in 0..1_000 {
        if parked() {
            return;
        }

        assert!(
            poll_once(call.as_mut()).await.is_pending(),
            "the call completed before it parked"
        );
    }

    panic!("the call never parked");
}

/// Polls `call` a few times, and it must still be waiting — for a lock the test holds, say.
pub async fn assert_still_waiting<F: Future>(mut call: Pin<&mut F>) {
    for _ in 0..10 {
        assert!(
            poll_once(call.as_mut()).await.is_pending(),
            "the call did not wait"
        );
    }
}

/// The file a call parks in, and whether it parks in a read or in a write.
pub struct Gate {
    pub file: Arc<InMemoryFile>,
    pub read: bool,
}

impl Gate {
    /// Parks every read of `role` at `offset`.
    pub fn reads_at(io: &InMemoryIo, role: FileRole, offset: u64) -> Self {
        let file = io.file(role).unwrap();
        file.shut_reads_at(offset);
        Self { file, read: true }
    }

    /// Parks every append and `write_at` of `role`.
    pub fn writes_of(io: &InMemoryIo, role: FileRole) -> Self {
        let file = io.file(role).unwrap();
        file.shut_writes();
        Self { file, read: false }
    }

    /// Lets the next `succeeding` appends and `write_at`s of `role` through, and parks the one after them.
    pub fn writes_of_after(io: &InMemoryIo, role: FileRole, succeeding: usize) -> Self {
        let file = io.file(role).unwrap();
        file.shut_writes_after(succeeding);
        Self { file, read: false }
    }

    pub fn is_waiting(&self) -> bool {
        if self.read {
            self.file.is_read_waiting()
        } else {
            self.file.is_write_waiting()
        }
    }

    pub fn open(&self) {
        self.file.open_reads();
        self.file.open_writes();
    }
}
