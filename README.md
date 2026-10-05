archived-data
=============

`archived-data` keeps archives that are **written once and read back by key**, from wherever their files live — a local directory or objects in S3. A service writes the rows it holds into a working archive in any order, compacts it into its final shape of exact size, uploads it, and from then on answers a read by key from the files where they lie: a few ranged reads, through page caches.

Nothing in the crate knows what is stored. The caller describes its key, its slot and its item with a few traits; the crate owns the pages, the indexes, the caches and the checks. Nothing in it knows where the bytes are either: an archive asks a factory for its files by ROLE — the index, data file `n` — and reads or writes each one through a file trait, so the same archive is written to local files and read out of S3 by swapping the factory. That is four small IO traits. The crate implements them over a local directory — `LocalFileIo` and the files it hands out; an S3 implementation is the service's to write, after the template in [io](docs/io.md).

It depends on `tokio` and `rust-extensions` and nothing else: there is no S3 client in it.

## Install

```toml
[dependencies]
archived-data = { tag = "${last_tag}", git = "https://github.com/MyJetTools/archived-data.git" }
rust-extensions = { tag = "0.1.5", git = "https://github.com/MyJetTools/rust-extensions.git" }
tokio = { version = "*", features = ["macros", "rt-multi-thread"] }
```

- `${last_tag}` stands for the repository's latest tag.
- `tokio`: the writers and the readers are `async`, `LocalFileIo` does its file IO on a tokio runtime, and the examples start one with `#[tokio::main]`, which takes these two features.
- `rust-extensions` is needed only with `fixed_key_minute_indexed_records` and `global_day_index`: they take and give `rust_extensions::date_time::DateTimeAsMicroseconds`, and the crate does not re-export it. Depend on it at the tag this crate uses, or the types differ.

## At a glance

One value per account per hour of a day: written in any order, compacted, read back by key.

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
    let root = std::env::temp_dir().join(format!("archived-data-docs-readme-1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let working = LocalFileIo::new(root.join("working"));
    let compact = LocalFileIo::new(root.join("compact"));

    // 1. Write the rows into a working archive, in any key order. Only `finish` says they are all in.
    let mut writer = FixedKeyPreallocatedSlotsArchiveWriter::new(working.clone()).await.unwrap();
    writer.write(Login(30), Hour(9), &Volume(300)).await.unwrap();
    writer.write(Login(10), Hour(9), &Volume(100)).await.unwrap();
    writer.write(Login(10), Hour(10), &Volume(110)).await.unwrap();
    writer.write(Login(20), Hour(23), &Volume(200)).await.unwrap();
    writer.finish().await.unwrap();

    // 2. Plan the compaction: the exact size of every file, known before a byte is written.
    let source = Reader::open(working).await.unwrap();
    let plan = source.compaction_plan().await.unwrap();
    assert_eq!(plan.keys_count(), 3);

    // 3. Compact into an empty place — another directory here, an S3 prefix in a service.
    source.write_compacted_to(&plan, &compact).await.unwrap();
    assert_eq!(std::fs::metadata(compact.index_path()).unwrap().len(), plan.index_size());

    // 4. Read the compact archive by key. A slot nobody wrote is the item's `Default`.
    let reader = Reader::open(compact).await.unwrap();
    let hours = reader.get(&Login(10), Hour(8), Hour(10)).await.unwrap();
    assert_eq!(hours, Some(vec![Volume(0), Volume(100), Volume(110)]));
    assert_eq!(reader.get(&Login(11), Hour(0), Hour(23)).await.unwrap(), None);

    // Every key between two bounds, in key order.
    let range = reader.get_range(&Login(20), &Login(30), Hour(9), Hour(9)).await.unwrap();
    assert_eq!(range, [(Login(20), vec![Volume(0)]), (Login(30), vec![Volume(300)])]);

    std::fs::remove_dir_all(&root).unwrap();
}
```

## Which shape for which data

| The data | Module | Read back by | Topic |
| --- | --- | --- | --- |
| One fixed-size value per key per period, every period known in advance; a fixed-size key | `fixed_key_preallocated_slots` | key and a range of slots; a range of keys | [fixed-key-preallocated-slots](docs/fixed-key-preallocated-slots.md) |
| The same, keyed by a name, a path, a composite id: 1 to 32 767 bytes, and 8 179 at most with the default page size | `variable_key_preallocated_slots` | the same, and every key under a prefix, for a key that keeps its bytes and sorts by them | [variable-key-preallocated-slots](docs/variable-key-preallocated-slots.md) |
| One day of variable-length records per account, in time order; a record is 8 to 8 192 bytes and starts with its time | `fixed_key_minute_indexed_records` | account and a time range | [fixed-key-minute-indexed-records](docs/fixed-key-minute-indexed-records.md) |
| Which days a login has data on, one file per server | `global_day_index` | login and a period | [global-day-index](docs/global-day-index.md) |

- The modules are private: every public name is at the crate root, `archived_data::Name`. The types to start from carry their module's name: `FixedKeyPreallocatedSlotsArchiveWriter` / `…Reader`, `VariableKeyPreallocatedSlotsArchiveWriter` / `…Reader`, `FixedKeyMinuteIndexedRecordsArchiveWriter` / `…Reader`, `GlobalDayIndex` / `GlobalDayIndexReader`.
- A limit to know before choosing: a day of records is WRITTEN through one data file of at most 4 GiB — 131 072 frames of 32 KiB, at least one per account. A `write` that needs one more frame panics, and so does a `write` of a record outside the day ([refusals, panics and failures](docs/fixed-key-minute-indexed-records.md#refusals-panics-and-failures)).

## The words used everywhere

| Word | Means |
| --- | --- |
| **role** | what an archive asks its factory for: the index, or data file `n`. It never learns a file name |
| **split archive** | the format of the three archive modules, the first three of the table above: ONE index and data files `0 ..= 255`, each file at most 4 GiB |
| **SingleFile** | the older one-file format of the two preallocated modules, kept because the objects already in S3 are in it |
| **day archive** | an archive of `fixed_key_minute_indexed_records`: the records of ONE day, whose start is given to its writer and to every reader of it |
| **working archive** | what a writer builds from rows in any key order. A split one opens only when its last flush completed and nothing reached its files after it |
| **compact archive** | what a compaction writes from a working archive: exact sizes, keys in order, the same bytes for the same content and the same `max_data_file_bytes`. The one that is kept |
| **frame** | the unit of a split archive's files: a header that starts `size u32 · next u32`, then a body. `next` chains a continuation frame |
| **L0** | the top of the index: intervals of keys, each naming an L1 page. A reader keeps all of it in memory |
| **L1 page** | the keys of one interval, ascending, each naming what the key owns: a data page, an L2 frame in a day archive, a login's year blocks in the global day index |
| **L2 frame** | day archives only: the 1440 minute cells of one key, each naming the first record of its minute |
| **data page** | a key's preallocated slots: `SLOTS × ITEM_LEN` bytes, a slot addressed by its position |
| **refusal** | an `Err(ContractViolation)` decided before anything changes: the writer stays usable |
| **failure** | any other `Err` of a writer, or a `write` / `flush` future dropped at an await: a split writer refuses every later `write`, `flush` and `finish`. A SingleFile writer has no such guard, and its file is unusable |

## What is inside

The crate is documented by topic. Each topic below links to a document in [`docs/`](docs) with the contracts and runnable examples. The examples are compiled and run by `cargo test --doc`, so they stay in step with the code.

### Storage shapes

- [**fixed-key-preallocated-slots**](docs/fixed-key-preallocated-slots.md) — a fixed-size key owns one preallocated page of fixed-size slots, and the slot's position is the second key: period `N` of a fixed calendar is slot `N`. The three traits a caller implements, the writer and where it puts a new key, the reader's `get` and `get_range`, the caches, the compact shape and its size formulas. It is also the reference for everything the two preallocated modules share.
- [**variable-key-preallocated-slots**](docs/variable-key-preallocated-slots.md) — the same archive with a key of 1 to 32 767 bytes: the length prefix, pages sized and split by bytes, the half-page rule that bounds a key by its page size, the index room a new key is checked for, compact pages packed by bytes, and every key that starts with a prefix.
- [**fixed-key-minute-indexed-records**](docs/fixed-key-minute-indexed-records.md) — ONE server-clock day: an account owns variable-length records in time order, in a chain of data frames, and a minute index of 1440 cells says where each minute starts. Writing records in any order, the refusals and the two panics of `write`, reading a time range, the minutes that hold records, compaction, and the decisions the format document left open.
- [**global-day-index**](docs/global-day-index.md) — one file per server: for every login, a bitmap of the days it has data on, by year, so a read over a period opens only the day archives that hold something. Building a new version in memory, and answering from the file where it lies.
- [**single-file**](docs/single-file.md) — the older one-file format of the two preallocated modules and its `…SingleFile` structures, over tokio's `AsyncRead` / `AsyncWrite` / `AsyncSeek`: the layout, the working file, the compact stream, the reader — `get`, and no `get_range` — and its caveats. New archives are split archives.

### How an archive lives

- [**lifecycle**](docs/lifecycle.md) — write, flush, finish, plan, compact, read. Why every file grows only by append, what a refusal and a failure leave behind, when a working archive opens and when a compact one is complete, what makes a compaction deterministic, and the cache defaults.
- [**split-format**](docs/split-format.md) — the bytes every split structure shares: the files of an archive, the three kinds of frame, the L0 chain and the hash that is the format's only identity, continuation frames, what `open` checks, the limits.
- [**io**](docs/io.md) — the four IO traits and their contracts, `LocalFileIo` over a local directory with everything it does beyond the contract, a complete in-memory implementation as the template for an S3 one, and what an S3 one must do.

### Recipes and small pieces

- [**archiving-a-day**](docs/archiving-a-day.md) — the archiver's recipe: build a day archive locally, compact it, upload the data files first and the index last, verify, set the day's bits in the global day index — in the order that lets any step be run again after a crash. And merging late rows into a day already archived: a rebuild, which is not atomic in place.
- [**pascal-strings**](docs/pascal-strings.md) — `write_pascal_string` / `read_pascal_string` for the strings inside a record: a length byte, at most 255 bytes, cut on a character boundary.
- [**errors**](docs/errors.md) — `ArchiveError` and what each variant means in every format; then, in one place, every refusal, every failure that stops a writer and every panic.

### For maintainers

- [**internals**](docs/internals.md) — the map of the source: how the code is layered, where a call goes, what every source file holds, which rules live in more than one file, the test infrastructure, what a change must keep true, and every public name of the crate in one doctest.

## For AI agents

[`docs/index_resource.md`](docs/index_resource.md) is the same map in compact form: every topic with the public names it documents, and the rules of every split archive. It is written to be served as the index of an MCP resource, with one resource per topic.
