# errors

Every archive call that can fail returns `ArchiveError`. The IO traits and `LocalFileIo` return `std::io::Result`, and their errors reach the archive's caller as `ArchiveError::Io`. The variant says what went wrong: the IO, the bytes, or the caller's side of a contract. This page also lists, in one place, what each writer refuses, what leaves a writer unusable, and every panic. Source: [error.rs](../src/error.rs).

| Variant | Means |
| --- | --- |
| `Io(std::io::Error)` | The sink, the source or a file of the IO traits failed. This includes a file the factory cannot create or open. |
| `Corrupted(String)` | The bytes are not this format, or not these sizes. For a compaction, it can also mean the source no longer matches its plan. |
| `ContractViolation(String)` | A trait implementation broke its contract, or a call would cross a limit of the format. It is also what a writer that a failure left unusable returns. |

The `String` is a message for a person to read. Code should match on the variant, not on the message.

## Io: the IO failed

- **Split structures**: both preallocated modules and the records module, `fixed_key_minute_indexed_records`, through the traits of [io](io.md).
  - An `io::Error` from an `ArchiveFileWriter`, `ArchiveFileReader`, `IoWriterFactory` or `IoReaderFactory` call is kept whole, with its kind and message. The messages of `LocalFileIo` name the file and what it was doing.
  - A file the factory cannot create or open is `Io` too. Over `LocalFileIo`, `open` with no index is `Io` of kind `NotFound`, and so is a `get` whose `data_ref` names a data file that does not exist.
  - A reader does not remember a data file that failed to open. The next read tries to open it again.
- **SingleFile** ([single-file](single-file.md)):
  - An `io::Error` of the sink or the source (`AsyncRead` / `AsyncWrite` / `AsyncSeek`).
  - A file that ends before the end of a data page a read asks for is `Io` of kind `UnexpectedEof`, whether it ends inside the page or before the page starts. So is a file that ends inside an L1 page after its 8-byte header.
  - `keys_count` reads each L1 header exactly, so it answers `UnexpectedEof` for an L1 header the file does not hold. `get` answers `Corrupted` for the same page.
  - The split readers clamp every read to the file's size and answer `Corrupted` for every one of these cuts.
- **Global day index** ([global-day-index](global-day-index.md)): an `io::Error` of its `ArchiveFileReader`. `read_sync_time` asks the reader for the first 8 bytes and nothing else, so on a shorter file it is the reader's `UnexpectedEof`.
- A reader stays usable after an `Io` error, and the same call can be tried again. On a writer, an `Io` error is a [failure](#failures-the-writer-refuses-every-later-call).

## Corrupted: the bytes are not what the reader expects

```rust
use archived_data::*;

/// An account id, 8 bytes little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Login(u64);

impl PrimaryKeyFixedSize for Login {
    const LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(u64::from_le_bytes(src.try_into().unwrap()))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
}

/// An hour of a day: 24 slots.
#[derive(Debug, Clone, Copy)]
struct Hour(usize);

impl PreallocatedSlotKey for Hour {
    const SLOTS: usize = 24;

    fn slot_index(&self) -> usize {
        self.0
    }
}

/// The volume traded in an hour; 0 is the empty slot.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Volume(u64);

impl PreallocatedDataItem for Volume {
    const ITEM_LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(u64::from_le_bytes(src.try_into().unwrap()))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
}

type Reader = FixedKeyPreallocatedSlotsArchiveReader<Login, Hour, Volume, LocalFileIo, LocalFileReader>;

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join(format!("archived-data-docs-errors-1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // No index at all: the factory cannot open it.
    let missing = Reader::open(LocalFileIo::new(&dir)).await.err().unwrap();
    assert!(matches!(&missing, ArchiveError::Io(error) if error.kind() == std::io::ErrorKind::NotFound));

    // An index written by hand is not a split index: its L0 header fails its hash.
    let io = LocalFileIo::new(&dir);
    let index = io.create_index_writer().await.unwrap();
    index.append(b"not an archive index").await.unwrap();
    index.finish().await.unwrap();
    assert!(matches!(Reader::open(io.clone()).await, Err(ArchiveError::Corrupted(_))));

    // A working archive whose writer has not completed a flush yet.
    let mut writer = FixedKeyPreallocatedSlotsArchiveWriter::new(io.clone()).await.unwrap();
    writer.write(Login(7), Hour(3), &Volume(100)).await.unwrap();
    assert!(matches!(Reader::open(io.clone()).await, Err(ArchiveError::Corrupted(_))));

    // Once a flush has completed, it opens.
    writer.flush().await.unwrap();
    let reader = Reader::open(io.clone()).await.unwrap();
    assert_eq!(reader.get(&Login(7), Hour(3), Hour(3)).await.unwrap(), Some(vec![Volume(100)]));

    writer.finish().await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
```

- **Split structures** ([what open checks](split-format.md#what-open-checks), [the frame-0 hash](lifecycle.md#when-a-working-archive-opens-the-frame-0-hash)):
  - an L0 header that fails its hash: random bytes, a SingleFile object, or a working index whose writer never completed a flush or wrote to its files after its last one;
  - a broken chain, or intervals that are not ordered and disjoint;
  - an index that ends before or inside its last L1 page;
  - an L1 page whose keys do not ascend or do not span its interval;
  - a frame that reaches past the end of its file;
  - a data page of other sizes;
  - in the records module ([fixed-key-minute-indexed-records](fixed-key-minute-indexed-records.md)): a read that meets a record outside the day the reader was given, records out of time order, a cell that does not point at the first record of its minute, or a chain that returns to a frame it walked. A WORKING index of this module can end with L2 frames, which `open` does not read: one cut inside a trailing L2 frame opens, and answers `Corrupted` at the first read of that key.
- **SingleFile**: a broken or cut L0 chain, intervals out of order, an L1 page that starts past the end of the file, or an L1 page or a data page of other sizes ([single-file](single-file.md)).
- **Global day index**: a file shorter than its sync time plus an L0 header (28 bytes), a header that fails its hash, a damaged L1 page or year block, or fewer than 8 bytes handed to `GlobalDayIndex::sync_time_of` ([global-day-index](global-day-index.md)).
- **Compaction**:
  - `compaction_plan` returns it for keys that do not ascend across L1 pages.
  - `write_compacted_to` returns it, with "the source changed while it was being compacted", for a source that no longer matches its plan.
- **Pascal strings**: a missing length, a short body, or bytes that are not UTF-8 ([pascal-strings](pascal-strings.md)).
- What to do: the bytes are wrong, and reading them again will not help. Rebuild a working archive from its source. Compact again to rebuild a compact one.
- One exception: a working archive whose writer is still running. As in the example above, it is `Corrupted` until its first flush completes, and again from the first byte that reaches a file after a flush. It opens once the next flush completes.

## ContractViolation: the caller's side of a contract

- **A trait implementation broke its contract.** The crate makes these checks itself, through crate-private traits with one blanket implementation each, so an implementation cannot override them away. Examples:
  - `PrimaryKeyFixedSize::copy_to` or `PreallocatedDataItem::copy_to` wrote other than `LEN` or `ITEM_LEN` bytes.
  - `PrimaryKeyVariableSize::copy_to` wrote 0 bytes, or more than `MAX_VARIABLE_KEY_LEN` = 32 767 bytes.
  - `PreallocatedSlotKey::slot_index` is not below `SLOTS`. A reader's `get` and `get_range` refuse this too, before they read anything.
  - `ArchiveFileWriter::append` returned an offset other than the end of the file.
- **A call would cross a limit of the format:**
  - a variable key too large for its L1 page (the half-page rule);
  - a split index that would grow past 4 GiB;
  - a data page that would need data file 256;
  - a compact index past 4 GiB, from `compaction_plan`;
  - a record shorter than 8 bytes or longer than 8 192 bytes;
  - one key's records longer than a compact data file;
  - a day in a year that a `u16` cannot hold (`GlobalDayIndex::set_day`);
  - a global day index past 4 GiB, or a login with more than 65 535 year blocks (`to_bytes`);
  - a prefix longer than 32 767 bytes (`last_key_starting_with`).
- **`write_compacted_to` was handed a plan of other sizes.** Only a size the plan carries is seen this way:
  - a plan whose data pages have another length (other slot or item types): refused at the first key, before any file is created;
  - a plan whose L0 frame has another length, which is a plan of the other preallocated module (fixed keys handed to a variable-key reader, or the reverse): refused after the data files are written and before the index is created;
  - in the records module, a plan of keys of another `LEN`: refused before any file is created.
  - Every other mismatch is `Corrupted`, "the source changed while it was being compacted". That covers a plan of fixed keys of another `LEN`, a plan of another archive, and any plan with another number of keys. This check can come before the ones above.
- **A writer that a failure left unusable** returns "an earlier write or flush failed or was dropped before it completed: the writer is not usable" for every later `write`, `flush` and `finish`. The two preallocated writers still answer a bad slot, item or variable key first (see the order of checks below).
- A writer's `ContractViolation` is a refusal only when it is in the [refusal list](#refusals-nothing-changed-the-writer-stays-usable). Any other `ContractViolation` from a writer is a failure.

## Display, source and conversions

```rust
use archived_data::*;
use std::error::Error;
use std::io::ErrorKind;

/// A producer that returns `io::Result`, such as an upload body, can call the archive with `?`.
fn comment_of(record: &[u8]) -> std::io::Result<String> {
    let (_symbol, rest) = read_pascal_string(record)?;
    let (comment, _) = read_pascal_string(rest)?;
    Ok(comment.to_string())
}

fn main() {
    let mut record = Vec::new();
    write_pascal_string("EURUSD", &mut record);
    write_pascal_string("closed by TP", &mut record);
    assert_eq!(comment_of(&record).unwrap(), "closed by TP");

    // Corrupted becomes InvalidData, with the ArchiveError inside.
    let error = comment_of(&record[..10]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "archive corrupted: a Pascal string states 12 bytes and 2 follow");
    let inner = error.get_ref().and_then(|inner| inner.downcast_ref::<ArchiveError>());
    assert!(matches!(inner, Some(ArchiveError::Corrupted(_))));

    // An io::Error becomes Io, and converts back unchanged.
    let missing = ArchiveError::from(std::io::Error::new(ErrorKind::NotFound, "no index"));
    assert_eq!(missing.to_string(), "archive io: no index");
    assert_eq!(missing.source().unwrap().to_string(), "no index");
    let back = std::io::Error::from(missing);
    assert_eq!((back.kind(), back.to_string()), (ErrorKind::NotFound, "no index".to_string()));

    // Corrupted and ContractViolation have no source.
    let refused = ArchiveError::ContractViolation("slot index 24 is not below SLOTS 24".to_string());
    assert_eq!(refused.to_string(), "archive contract violation: slot index 24 is not below SLOTS 24");
    assert!(refused.source().is_none());

    // Send + Sync + 'static: it boxes like any other error.
    let boxed: Box<dyn Error + Send + Sync> = Box::new(refused);
    assert!(boxed.downcast_ref::<ArchiveError>().is_some());
}
```

| Variant | `Display` | `std::error::Error::source` | `std::io::Error::from` |
| --- | --- | --- | --- |
| `Io(err)` | `archive io: {err}` | `Some(err)` | `err` itself, with its kind and message |
| `Corrupted(message)` | `archive corrupted: {message}` | `None` | kind `InvalidData`, with the `ArchiveError` inside |
| `ContractViolation(message)` | `archive contract violation: {message}` | `None` | kind `InvalidData`, with the `ArchiveError` inside |

- `impl From<std::io::Error> for ArchiveError` gives `Io`, so `?` on an `io::Result` works in a function that returns `Result<_, ArchiveError>`.
- `impl From<ArchiveError> for std::io::Error` exists so that a producer returning `io::Result`, such as a service's upload writer, can use `?` on an archive call. `Io` gives back the error it wraps, so `NotFound` and `UnexpectedEof` keep their kinds. The other two variants become `InvalidData`, because nothing failed in the IO: the bytes, or what the caller passed in, are not valid. The `io::Error` displays as the `ArchiveError`, and `get_ref()` or `into_inner()` downcasts back to it.
- `ArchiveError` derives `Debug` only. It is neither `Clone` nor `PartialEq`, so compare it with `matches!`. It is `Send + Sync + 'static`.

## A failed read is never an absent key

- A reader answers "absent" only when the archive says the key is not there. That answer is `Ok(None)` from `get`, a list without the key from `get_range`, or an empty list from the record reads and `days_with_data`.
- A failed read, a damaged page, or a data file that cannot be opened is always `Err`. It is never `None`, never a shorter list, and never a partial `Ok`.
- `get` with `from > to` returns `Ok(Some(vec![]))` at once, without reading anything. It says nothing about whether the key is there.

## Limits: the L0 chain and the split index

- An L0 chain has no limit of its own: more intervals chain more frames ([limits](split-format.md#limits)).
- SingleFile offsets are `u64`, so nothing ever says a SingleFile index is full.
- The split index as a whole is capped at `MAX_INDEX_BYTES` = 4 GiB by its `u32` offsets.
  - Before a new key changes anything, a writer works out the index bytes it may need: an L1 page, the L0 continuations the next flush will need (for variable keys, an upper bound on them), and the L2 frame of a records key.
  - A key that would take the index past the cap is refused with `ContractViolation`. So a flush never runs into the limit.
- `MAX_DATA_FILES` = 256 and `MAX_DATA_FILE_BYTES` = 4 GiB are field widths too. What happens at each differs:
  - A preallocated writer never crosses a data file's size: a data page that does not fit whole starts the next data file. So only a page that needs data file 256 is refused, as `ContractViolation`.
  - The records writer has ONE working data file. A data frame that would take it past 4 GiB is a [panic](#panics), not an error.
  - A compaction plan refuses data that needs data file 256 and, for records, one key's records longer than a data file, as `ContractViolation`.

## Every outcome in one place

A writer call that does not return `Ok` ends in one of three ways:

| Outcome | What comes back | The writer after it |
| --- | --- | --- |
| refusal | a `ContractViolation` from the lists below | Usable. Nothing the call asked for happened. |
| failure | any other `Err`, or a `write` or `flush` future dropped at an await before it returned | A split writer refuses every later `write`, `flush` and `finish`. A SingleFile writer's file is unusable. |
| panic | nothing: the call does not return | The precondition broken and the reason are in the panic tables below. |

### Refusals: nothing changed, the writer stays usable

```rust
use archived_data::*;

/// An account id, 8 bytes little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Login(u64);

impl PrimaryKeyFixedSize for Login {
    const LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(u64::from_le_bytes(src.try_into().unwrap()))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
}

/// An hour of a day: 24 slots.
#[derive(Debug, Clone, Copy)]
struct Hour(usize);

impl PreallocatedSlotKey for Hour {
    const SLOTS: usize = 24;

    fn slot_index(&self) -> usize {
        self.0
    }
}

/// The volume traded in an hour; 0 is the empty slot.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Volume(u64);

impl PreallocatedDataItem for Volume {
    const ITEM_LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(u64::from_le_bytes(src.try_into().unwrap()))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
}

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join(format!("archived-data-docs-errors-2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut writer = FixedKeyPreallocatedSlotsArchiveWriter::new(LocalFileIo::new(&dir)).await.unwrap();
    writer.write(Login(7), Hour(3), &Volume(100)).await.unwrap();

    // Hour(24) is not below SLOTS: refused before anything changed.
    let refused = writer.write(Login(7), Hour(24), &Volume(1)).await.unwrap_err();
    assert!(matches!(refused, ArchiveError::ContractViolation(_)));
    assert_eq!(refused.to_string(), "archive contract violation: slot index 24 is not below SLOTS 24");

    // The writer is still usable.
    writer.write(Login(8), Hour(0), &Volume(5)).await.unwrap();
    writer.finish().await.unwrap();

    let reader = FixedKeyPreallocatedSlotsArchiveReader::<Login, Hour, Volume, _, _>::open(LocalFileIo::new(&dir))
        .await
        .unwrap();
    assert_eq!(reader.get(&Login(7), Hour(3), Hour(3)).await.unwrap(), Some(vec![Volume(100)]));
    assert_eq!(reader.get(&Login(8), Hour(0), Hour(0)).await.unwrap(), Some(vec![Volume(5)]));
    assert_eq!(reader.keys_count().await.unwrap(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}
```

Every refusal is a `ContractViolation`.

| Writer | `write` refuses |
| --- | --- |
| `FixedKeyPreallocatedSlotsArchiveWriter` ([writer.rs](../src/fixed_key_preallocated_slots/writer.rs)) | a slot index not below `SLOTS` ([preallocated_slot_key.rs](../src/preallocated_slot_key.rs)); an item whose `copy_to` wrote other than `ITEM_LEN` bytes ([preallocated_data_item.rs](../src/preallocated_data_item.rs)); a new key whose `copy_to` wrote other than `LEN` bytes ([primary_key.rs](../src/fixed_key_preallocated_slots/primary_key.rs)); a new key that needs data file 256 ([data_file_layout.rs](../src/data_file_layout.rs)); a new key that needs an L1 page, plus the L0 continuations the next flush will need for it, past 4 GiB of index ([split_index_writer.rs](../src/split_index_writer.rs)) |
| `VariableKeyPreallocatedSlotsArchiveWriter` ([writer.rs](../src/variable_key_preallocated_slots/writer.rs)) | a slot index not below `SLOTS`; an item's byte count; a key whose `copy_to` wrote 0 bytes or more than 32 767 bytes ([primary_key.rs](../src/variable_key_preallocated_slots/primary_key.rs)); "key too large", meaning `2 × (prefix + key + 5) > l1_page_bytes − 12`, so 8 179 bytes at most with 16 KiB pages ([split_layout.rs](../src/variable_key_preallocated_slots/split_layout.rs)); a new key that needs data file 256; a new key that needs index bytes past 4 GiB, which covers a new L1 page and the L0 continuations, bounded rather than counted. A key longer than every key before it can be refused for a continuation even when it needs no page. Near the limit the bound can refuse a key a flush would still have had room for; it never accepts one a flush has no room for. |
| `FixedKeyMinuteIndexedRecordsArchiveWriter` ([writer.rs](../src/fixed_key_minute_indexed_records/writer.rs)) | a record shorter than `RECORD_TIME_LEN` = 8 bytes or longer than `MAX_RECORD_LEN` = 8 192 bytes ([layout.rs](../src/fixed_key_minute_indexed_records/layout.rs)); a new key whose `copy_to` wrote other than `LEN` bytes; a new key that needs index room past 4 GiB. That room is its L2 frame (12 968 bytes), an L1 page if it starts one, and the L0 continuations the next flush will need, and it is counted for every new key ([writer_key.rs](../src/fixed_key_minute_indexed_records/writer_key.rs)). |
| `FixedKeyPreallocatedSlotsArchiveWriterSingleFile` ([writer_single_file.rs](../src/fixed_key_preallocated_slots/writer_single_file.rs)) | a slot index not below `SLOTS`; an item's byte count. Nothing else. A key whose `copy_to` wrote other than `LEN` bytes is accepted, so it is not a refusal: see the [failures](#failures-the-writer-refuses-every-later-call). |
| `VariableKeyPreallocatedSlotsArchiveWriterSingleFile` ([writer_single_file.rs](../src/variable_key_preallocated_slots/writer_single_file.rs)) | a slot index not below `SLOTS`; an item's byte count; a key whose `copy_to` wrote 0 bytes or more than 32 767 bytes; "key too large", meaning `2 × (prefix + key + 8) > l1_page_bytes − 8`, so 8 178 bytes at most with 16 KiB pages ([layout.rs](../src/variable_key_preallocated_slots/layout.rs)) |

- **When each check runs:**
  - The slot, the item, a variable key and a record's length are checked before any IO, so a call refused there read and wrote nothing.
  - A new fixed key's `copy_to` and the data-file and index-room refusals come after the writer has read the pages involved into its cache. As after any write, the L1 cache is then evicted down to its size, and in the records writer the L2 and data-frame caches too.
  - That eviction adds nothing of the refused call to the archive, but it can send a page that an EARLIER write changed to its file. Like any byte written after a flush, this leaves the working archive `Corrupted` at `open` until the next flush completes.
  - If the eviction fails, the call returns the eviction's error instead, and that is a failure.
- **Order of checks:** `FixedKeyPreallocatedSlotsArchiveWriter` and `VariableKeyPreallocatedSlotsArchiveWriter` check the slot, the item and the variable key before they check whether an earlier failure left them unusable. The records writer checks that first.
- **Other calls that refuse and change nothing:**
  - a reader's `get` or `get_range` with a slot index not below `SLOTS`;
  - `compaction_plan` with a compact index past 4 GiB, data that needs data file 256, or one key's records longer than a compact data file;
  - `GlobalDayIndex::set_day` with a day in a year that a `u16` cannot hold;
  - `GlobalDayIndex::to_bytes` with a file past 4 GiB, more than 65 535 year blocks for one login, or a broken key `copy_to`;
  - `last_key_starting_with` with a prefix longer than 32 767 bytes.

### Failures: the writer refuses every later call

A failure can carry the same variant, and even the same message, as a refusal. Here the item written is fine, but the EMPTY item that fills a new data page breaks its contract.

```rust
use archived_data::*;

/// An account id, 8 bytes little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Login(u64);

impl PrimaryKeyFixedSize for Login {
    const LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(u64::from_le_bytes(src.try_into().unwrap()))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
}

/// An hour of a day: 24 slots.
#[derive(Debug, Clone, Copy)]
struct Hour(usize);

impl PreallocatedSlotKey for Hour {
    const SLOTS: usize = 24;

    fn slot_index(&self) -> usize {
        self.0
    }
}

/// The volume traded in an hour. Its empty value is BROKEN: `Default` writes 7 bytes, not `ITEM_LEN`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Volume(Option<u64>);

impl PreallocatedDataItem for Volume {
    const ITEM_LEN: usize = 8;

    fn from_slice(src: &[u8]) -> Self {
        Self(Some(u64::from_le_bytes(src.try_into().unwrap())))
    }

    fn copy_to(&self, out: &mut Vec<u8>) {
        match self.0 {
            Some(volume) => out.extend_from_slice(&volume.to_le_bytes()),
            None => out.extend_from_slice(&[0; 7]),
        }
    }
}

const UNUSABLE: &str = "archive contract violation: an earlier write or flush failed or was dropped \
                        before it completed: the writer is not usable";

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join(format!("archived-data-docs-errors-3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut writer = FixedKeyPreallocatedSlotsArchiveWriter::new(LocalFileIo::new(&dir)).await.unwrap();

    // The empty item is built with the first new key's data page, after data file 0 was created: a failure.
    let failed = writer.write(Login(7), Hour(3), &Volume(Some(100))).await.unwrap_err();
    assert_eq!(
        failed.to_string(),
        "archive contract violation: PreallocatedDataItem::copy_to wrote 7 bytes, ITEM_LEN is 8"
    );
    assert_eq!((writer.keys_count(), writer.data_files()), (0, 1));

    // Every later write and flush is refused.
    let again = writer.write(Login(7), Hour(3), &Volume(Some(100))).await.unwrap_err();
    assert_eq!(again.to_string(), UNUSABLE);
    assert_eq!(writer.flush().await.unwrap_err().to_string(), UNUSABLE);

    // A slot out of range is checked before that, and still gets its own refusal.
    let slot = writer.write(Login(7), Hour(24), &Volume(Some(1))).await.unwrap_err();
    assert_eq!(slot.to_string(), "archive contract violation: slot index 24 is not below SLOTS 24");

    // finish is refused too, and still finishes the index and the data file.
    assert_eq!(writer.finish().await.unwrap_err().to_string(), UNUSABLE);

    // No flush ever completed, so the working archive does not open.
    let opened =
        FixedKeyPreallocatedSlotsArchiveReader::<Login, Hour, Volume, _, _>::open(LocalFileIo::new(&dir)).await;
    assert!(matches!(opened, Err(ArchiveError::Corrupted(_))));

    std::fs::remove_dir_all(&dir).unwrap();
}
```

**Split writers**: `FixedKeyPreallocatedSlotsArchiveWriter`, `VariableKeyPreallocatedSlotsArchiveWriter` and `FixedKeyMinuteIndexedRecordsArchiveWriter`.

- **What counts as a failure:**
  - any `Err` of `write`, `flush` or `finish` that is not a refusal: `Io`, or `Corrupted` because the writer reads its own pages back;
  - a `ContractViolation` that is not in the lists above. One example is a `TItem::default()` whose `copy_to` writes other than `ITEM_LEN` bytes: it is found when the first new key's data page is built, after the page was reserved and data file 0 created. Another is an `ArchiveFileWriter::append` that returned an offset other than the end of the file;
  - a `write` or `flush` future dropped at an await, before it returned. Neither is cancel-safe. This holds even when the dropped call had changed nothing yet. A future that was never polled did nothing, and the writer stays usable.
- **Every later `write`, `flush` and `finish` is refused** with `ContractViolation("an earlier write or flush failed or was dropped before it completed: the writer is not usable")`. [in_flight_guard.rs](../src/in_flight_guard.rs) marks a call in flight on the way in. It clears the mark only when the call returns `Ok`, or returns a refusal that changed nothing. A dropped future runs no error path, which is why the mark is set on the way in.
- **`finish` still finishes every file.** It calls `finish()` on the index and on every data file it created, and awaits each one even after another has failed. It returns the flush's error first, then the first failed finish ([finish_split_writers.rs](../src/finish_split_writers.rs)).
- **The working archive is in an unknown state.** It opens only if its last flush completed and nothing reached its files after it, and then it holds what that flush saved. Otherwise `open` is `Corrupted`. Rebuild it from its source ([the frame-0 hash](lifecycle.md#when-a-working-archive-opens-the-frame-0-hash)).

**SingleFile writers**: `FixedKeyPreallocatedSlotsArchiveWriterSingleFile` and `VariableKeyPreallocatedSlotsArchiveWriterSingleFile`.

- Every `Err` that is not a refusal leaves the file unusable, and so does a `write` or `flush` future dropped at an await before it returned.
- These writers have no guard: they do not refuse later calls. The caller must stop calling the writer after a failure.
- The format has no hash. A file whose writer stopped anywhere except at a completed `flush` may still open and answer what no flush saved. It is a temporary file: rebuild it ([what a stopped writer leaves](single-file.md#flush-finish-and-what-a-stopped-writer-leaves)).
- The fixed one does not check a new key's `copy_to` in `write`. A key that writes other than `LEN` bytes is accepted, and the eviction or the `flush` that encodes its L1 page then fails with `ContractViolation`.
- A `TItem::default()` whose `copy_to` writes other than `ITEM_LEN` bytes is a failure in both, as in the split writers: it is found after the new key's data page was allocated. Its message is the one a refused item gets.

**Compaction**, `write_compacted_to`:

- An `Err` in the data pass creates no index: the index is created only once every data file is finished. A place that was empty then has no index, and `open` fails with `Io`.
- An index that an earlier run left in the same place is not removed by a failed data pass. It still opens, over data files the failed run may have partly replaced. So compact into an empty place, and never into the place the reader itself reads.
- An `Err` in the index pass can leave a short index, which `open` refuses as `Corrupted`.
- Run it again: it replaces every file it creates. A compact archive is complete only when its index opens ([compaction](lifecycle.md#compaction-the-data-files-first-the-index-last)).

**Readers**: an `Err` changes nothing. The reader stays usable, and the call can be tried again. A dropped call is harmless too, with one exception: a SingleFile reader over a source that keeps a read in flight. A `get`, `keys_count`, `compacted_len` or `write_compacted_to` dropped in the middle of a read leaves that shared source mid-request, and the next caller's seek fails. `tokio::fs::File` is not such a source ([reading](single-file.md#reading)).

### Panics

Every `panic!`, `assert!`, `expect` and `unwrap` in non-test code is one of the following. There is no `unreachable!`, `todo!` or `unimplemented!`.

- a construction-time size check;
- a setting made out of range or at the wrong time;
- a record outside the writer's day, or a working data file of records that would pass 4 GiB;
- a `LocalFileWriter` used after it was finished, or dropped unfinished when its sync fails;
- an internal invariant.

None of them is a state to recover from.

**Construction checks.** A type or a size that fails one could never be written, so it is refused before the first byte is written or read. The data-page checks are `SLOTS` 0, `ITEM_LEN` 0 or above 65 535, or a data page past `u32::MAX`.

The sizes are computed in `usize`, and only the split data-page check (`10 + SLOTS × ITEM_LEN`) uses checked arithmetic. In the other checks, a product that overflows `usize` itself (a `SLOTS` or a capacity of the order of 2⁶⁰ on a 64-bit target) panics with "attempt to multiply with overflow" in a build with overflow checks. In a release build it wraps, and a wrapped size can pass the check.

| Call | Panics when |
| --- | --- |
| `FixedKeyPreallocatedSlotsArchiveWriter::new`, `new_with_capacity` | Any of: `LEN` 0; the data-page checks for `10 + SLOTS × ITEM_LEN`; `capacity_l0` or `capacity_l1` 0; an L1 page `12 + max(capacity_l1, 1024) × (LEN + 5)` past `u32::MAX`; an L0 frame `20 + capacity_l0 × (2·LEN + 4)` past `u32::MAX`. See [split_layout.rs](../src/fixed_key_preallocated_slots/split_layout.rs) and [split_data_page.rs](../src/split_data_page.rs). |
| `FixedKeyPreallocatedSlotsArchiveReader::open` | The same checks, with `capacity_l0` 1 and `capacity_l1` 1024. |
| `VariableKeyPreallocatedSlotsArchiveWriter::new`, `new_with_capacity` | Any of: the data-page checks for `10 + SLOTS × ITEM_LEN`; `l1_page_bytes` below 26 (two entries of a one-byte key); `l1_page_bytes` or `l0_frame_bytes` past `u32::MAX`; `l0_frame_bytes` below `20 + 2 × min((l1_page_bytes − 12) / 2 − 5, 2 + 32 767) + 4`, which is 16 386 for 16 KiB pages. See [split_layout.rs](../src/variable_key_preallocated_slots/split_layout.rs). |
| `VariableKeyPreallocatedSlotsArchiveReader::open` | The data-page checks for `10 + SLOTS × ITEM_LEN`. |
| `FixedKeyPreallocatedSlotsArchiveWriterSingleFile::new`, `new_with_capacity` | Any of: `LEN` 0; the data-page checks for `14 + SLOTS × ITEM_LEN`; a capacity 0; an L1 page `8 + max(capacity_l1, 1024) × (LEN + 8)` past `u32::MAX`; an L0 frame `16 + capacity_l0 × (2·LEN + 8)` past `u32::MAX`. See [layout.rs](../src/fixed_key_preallocated_slots/layout.rs) and [data_page.rs](../src/data_page.rs). |
| `FixedKeyPreallocatedSlotsArchiveReaderSingleFile::open` | The same checks, with 1 and 1024. |
| `VariableKeyPreallocatedSlotsArchiveWriterSingleFile::new`, `new_with_capacity` | Any of: the data-page checks for `14 + SLOTS × ITEM_LEN`; `l1_page_bytes` below 28; a size past `u32::MAX`; `l0_frame_bytes` below `16 + 2 × min((l1_page_bytes − 8) / 2 − 8, 2 + 32 767) + 8`, which is 16 384 for 16 KiB pages. See [layout.rs](../src/variable_key_preallocated_slots/layout.rs). |
| `VariableKeyPreallocatedSlotsArchiveReaderSingleFile::open` | The data-page checks for `14 + SLOTS × ITEM_LEN`. |
| `FixedKeyMinuteIndexedRecordsArchiveWriter::new`, `new_with_capacity` | Any of: `LEN` 0; a capacity 0; an L1 page `12 + max(capacity_l1, 1024) × (LEN + 4)` past `u32::MAX`; an L0 frame `20 + capacity_l0 × (2·LEN + 4)` past `u32::MAX`. See [layout.rs](../src/fixed_key_minute_indexed_records/layout.rs). |
| `FixedKeyMinuteIndexedRecordsArchiveReader::open` | The same checks, with 1 and 1024. |
| `GlobalDayIndexReader::open`, and through it `GlobalDayIndex::from_bytes` and `read_from` | `LEN` 0, or an L1 page `12 + 1024 × (LEN + 6)` past `u32::MAX`. See [reader.rs](../src/global_day_index/reader.rs). |

**Settings.**

| Call | Panics when |
| --- | --- |
| `set_max_data_file_bytes` of `FixedKeyPreallocatedSlotsArchiveWriter` and `VariableKeyPreallocatedSlotsArchiveWriter` | Any of these, checked in this order: a key has been written ("the data file size cannot change once a key is written"); `bytes` is above `MAX_DATA_FILE_BYTES` (4 GiB) or below one data page; data file 0 exists ("the data file size cannot change once a data page is placed"). The last one is reached only with no key written: the `write` of the archive's first key failed, or was dropped, after it created data file 0. See [split_limits.rs](../src/split_limits.rs) and [split_data_writers.rs](../src/split_data_writers.rs). |
| `set_max_data_file_bytes` of `FixedKeyPreallocatedSlotsArchiveReader` and `VariableKeyPreallocatedSlotsArchiveReader` | Above 4 GiB, or below one data page. |
| `FixedKeyMinuteIndexedRecordsArchiveReader::set_max_data_file_bytes` | Above 4 GiB, or below one data frame (`DATA_FRAME_LEN` = 32 KiB). |

**A record outside its day, and a working data file past 4 GiB.** Both are panics of `FixedKeyMinuteIndexedRecordsArchiveWriter::write` ([writer.rs](../src/fixed_key_minute_indexed_records/writer.rs)), and both are what its format document asks for ([fixed-key-minute-indexed-records](fixed-key-minute-indexed-records.md)). Both checks below fire before the record changes anything, and only on a usable writer: a writer that an earlier failure left unusable refuses first.

| Condition | Message |
| --- | --- |
| The record's time is outside `[day_start, day_start + 1440 minutes)`. The length is checked first, so a record shorter than 8 bytes is a refusal. | `archive {name}: a record at {time} ({µs} µs) is outside its day, which starts at {day_start} ({µs} µs)` |
| A data frame would take the data file past 4 GiB. | `archive {name}: its data file is {size} bytes, and {n} more for a data frame would take it past {max} bytes` |

The first check runs before the call is marked in flight, and the second after it. So a writer whose panic was caught is still usable after the first, and refuses every later call after the second ([fixed-key-minute-indexed-records](fixed-key-minute-indexed-records.md#refusals-panics-and-failures)).

**A finished file**: `LocalFileWriter` ([writer.rs](../src/local_file_io/writer.rs)).

| Condition | Message |
| --- | --- |
| `append`, `write_at` or `size` after `finish`. A second `finish` is `Ok(())`. | `the archive file {path} is already finished: nothing can be written to it` |
| `Drop` of a writer that was never finished, when the sync it runs fails | `the finish of an archive file dropped unfinished failed: sync {path}: {error}` |

The `Drop` panic happens where the sync runs, because nobody is left to return the error to. That is a blocking-pool thread, or a thread of its own when the writer is dropped outside a tokio runtime, having outlived the one it was made in. The panic hook reports it, and it does not unwind into the code that dropped the writer. With `panic = "abort"`, it aborts the process.

**Internal invariants.** Each of these restates an invariant the crate keeps on its own, so it fires only on a bug in the crate. Two rows can also be reached from outside. An `Ord` of a key that answers differently between two comparisons of the same keys can reach an `unwrap_err()`. An `ArchiveFileReader` that breaks its exact-read contract reaches the last row:

| Where | Expression |
| --- | --- |
| the key placement of the three split writers ([writer.rs](../src/fixed_key_preallocated_slots/writer.rs), [writer.rs](../src/variable_key_preallocated_slots/writer.rs), [writer_key.rs](../src/fixed_key_minute_indexed_records/writer_key.rs)) | `expect("a placed key's pages are cached")`, `expect("an L1 page just loaded is cached")`: the write read the pages into its cache and evicts nothing until the key is placed |
| [split_writer_data_pages.rs](../src/split_writer_data_pages.rs) | `expect("a data page past the append frontier is always cached")` |
| [writer_data_frames.rs](../src/fixed_key_minute_indexed_records/writer_data_frames.rs) | `expect("a data frame in use is cached")`, `expect("a frame is allocated only once the data file exists")` |
| [writer_index_pages.rs](../src/fixed_key_minute_indexed_records/writer_index_pages.rs), [writer_insert.rs](../src/fixed_key_minute_indexed_records/writer_insert.rs) | `expect("a dirty page is cached")`, `expect("the L2 frame of a key being written is cached")` |
| [records_frame.rs](../src/fixed_key_minute_indexed_records/records_frame.rs) | `RecordsFrame::insert` asserts that the bytes fit the frame; the insert was planned so they do |
| [split_l0_chain.rs](../src/split_l0_chain.rs) | `expect("a lay-out has frame 0")` |
| the writers, the compaction and the records read-ahead | `binary_search_by(…).unwrap_err()` for a key the same call found absent; `unwrap()` of a value set a few lines before: the empty data page, the reader of the index or of the data file, the compact data file being written, the last L0 body, the last planned L1 page, the read-ahead window just read; the SingleFile writers' `get_mut(…).unwrap()` of an L1 page just loaded or just stored |
| [le_bytes.rs](../src/le_bytes.rs), `record_time` | `try_into().unwrap()` of a slice just cut to the exact length, out of bytes whose length the caller checked |
| [layout.rs](../src/fixed_key_minute_indexed_records/layout.rs) | `const _: () = assert!(…)`, which is checked at compile time |
| `debug_assert!`, `debug_assert_eq!`, `debug_assert_ne!` | only in debug builds |
| [blocking_file_ops.rs](../src/local_file_io/blocking_file_ops.rs) | `run_blocking_file_op` resumes a panic raised inside a blocking file operation and has no panic of its own |
| the readers, through [sized_reader.rs](../src/sized_reader.rs) | `read_at` must return exactly `len` bytes, and the readers do not check the length they get back. A `read_at` that returns fewer bytes can make a decoder slice past them and panic, for example while decoding an L1 page, or in `sync_time_of` of [reader.rs](../src/global_day_index/reader.rs), which slices the 8 bytes `GlobalDayIndexReader::open` and `read_sync_time` asked for |
