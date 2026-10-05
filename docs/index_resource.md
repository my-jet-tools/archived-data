# archived-data — index

Archives that are written once and read back by key, from local files or objects in S3. Generic over what is stored: the caller implements traits for its key, slot and item. A split archive — the format of the first three modules below — reaches its files by role, the index or data file `n` of `0 ..= 255`, through two factory traits, so one factory writes local files and another reads S3. The crate ships the local one, `LocalFileIo`, and no S3 client: an S3 factory is the service's to write.

Read a topic: `docs/{topic}.md`. Every topic has the contracts and runnable examples; the examples are compiled and run by `cargo test --doc`.

```toml
archived-data = { tag = "${last_tag}", git = "https://github.com/MyJetTools/archived-data.git" }
rust-extensions = { tag = "0.1.5", git = "https://github.com/MyJetTools/rust-extensions.git" }
tokio = { version = "*", features = ["macros", "rt-multi-thread"] }
```

Depends on `tokio` and `rust-extensions` only. The writers and the readers are `async`, and `LocalFileIo` needs a tokio runtime, which the examples start with `#[tokio::main]`. `DateTimeAsMicroseconds` in the API of `fixed_key_minute_indexed_records` and `global_day_index` is `rust_extensions::date_time::DateTimeAsMicroseconds` and is not re-exported: the caller depends on `rust-extensions` at the same tag. The modules are private: every public name is at the crate root, `archived_data::Name`.

| The data | Module | Topic |
| --- | --- | --- |
| One fixed-size value per key per period, a fixed-size key | `fixed_key_preallocated_slots` | `fixed-key-preallocated-slots` |
| The same, a key of 1 to 32 767 bytes — 8 179 at most with the default page size | `variable_key_preallocated_slots` | `variable-key-preallocated-slots` |
| One day of variable-length records per account, in time order; a record is 8 to 8 192 bytes, its time first | `fixed_key_minute_indexed_records` | `fixed-key-minute-indexed-records` |
| Which days a login has data on, one file per server | `global_day_index` | `global-day-index` |

Every public name of the crate stands once in the last column, beside the topic that documents it.

| Topic | What is inside | Public names |
| --- | --- | --- |
| [`fixed-key-preallocated-slots`](fixed-key-preallocated-slots.md) | A key owns one preallocated page of slots: the three traits, the writer and where a new key goes, `get` / `get_range` / `keys_count`, caches, the compact shape. The reference for what both preallocated modules share | `PrimaryKeyFixedSize`, `PreallocatedSlotKey`, `PreallocatedDataItem`, `FixedKeyPreallocatedSlotsArchiveWriter`, `FixedKeyPreallocatedSlotsArchiveReader`, `DEFAULT_L0_CAPACITY`, `DEFAULT_L1_CAPACITY`, `COMPACT_L1_CAPACITY` |
| [`variable-key-preallocated-slots`](variable-key-preallocated-slots.md) | What a variable key changes: the length prefix, pages in bytes, the half-page rule, index room, compact pages by bytes, every key under a prefix | `PrimaryKeyVariableSize`, `VariableKeyPreallocatedSlotsArchiveWriter`, `VariableKeyPreallocatedSlotsArchiveReader`, `last_key_starting_with`, `MAX_VARIABLE_KEY_LEN`, `DEFAULT_L0_FRAME_BYTES`, `DEFAULT_L1_PAGE_BYTES`, `COMPACT_L1_MAX_KEYS`, `COMPACT_L1_PAGE_BYTES` |
| [`fixed-key-minute-indexed-records`](fixed-key-minute-indexed-records.md) | One day: records of a key in time order, a minute index of 1440 cells; the writer, its refusals and its two panics, reader by time range, compaction, the decisions the format document left open | `FixedKeyMinuteIndexedRecordsArchiveWriter`, `FixedKeyMinuteIndexedRecordsArchiveReader`, `MinuteIndexedRecordsCompactionPlan`, `MINUTES_PER_DAY`, `RECORD_TIME_LEN`, `MAX_RECORD_LEN`, `DATA_FRAME_LEN` |
| [`global-day-index`](global-day-index.md) | One file per server: per login, the days with data. A version in memory, and lookups from the file where it lies | `GlobalDayIndex`, `GlobalDayIndexReader`, `GLOBAL_DAY_INDEX_SYNC_TIME_LEN` |
| [`single-file`](single-file.md) | The older one-file format of the two preallocated modules, over tokio's async IO traits; its reader has `get` and no `get_range` | `FixedKeyPreallocatedSlotsArchiveWriterSingleFile`, `FixedKeyPreallocatedSlotsArchiveReaderSingleFile`, `VariableKeyPreallocatedSlotsArchiveWriterSingleFile`, `VariableKeyPreallocatedSlotsArchiveReaderSingleFile` |
| [`lifecycle`](lifecycle.md) | Write, flush, finish, plan, compact, read: the append frontier, refusals and failures, when a working archive opens, when a compact one is complete, determinism, cache defaults | `CompactionPlan`, `DEFAULT_READER_L1_CACHE_PAGES`, `DEFAULT_READER_DATA_CACHE_PAGES`, `DEFAULT_READER_L2_CACHE_FRAMES`, `DEFAULT_WRITER_L1_CACHE_PAGES`, `DEFAULT_WRITER_DATA_CACHE_PAGES`, `DEFAULT_WRITER_L2_CACHE_FRAMES` |
| [`split-format`](split-format.md) | The bytes every split structure shares: files, frames, the L0 chain and its hash, continuations, what `open` checks, limits | `MAX_INDEX_BYTES`, `MAX_DATA_FILES`, `MAX_DATA_FILE_BYTES`, `DEFAULT_MAX_DATA_FILE_BYTES` |
| [`io`](io.md) | The four IO traits and their contracts, `LocalFileIo`, an in-memory implementation as the template for S3, what an S3 one must do | `ArchiveFileWriter`, `ArchiveFileReader`, `IoWriterFactory`, `IoReaderFactory`, `LocalFileIo`, `LocalFileWriter`, `LocalFileReader`, `index_file_name`, `data_file_name` |
| [`archiving-a-day`](archiving-a-day.md) | The archiver's recipe: a day of `fixed_key_minute_indexed_records` built locally, compacted, uploaded, verified, the global day index updated, in the order that lets any step be run again after a crash; merging late rows into a day already archived | — |
| [`pascal-strings`](pascal-strings.md) | Strings inside a record: a length byte, at most 255 bytes, cut on a character boundary | `write_pascal_string`, `read_pascal_string`, `PASCAL_STRING_MAX_BYTES` |
| [`errors`](errors.md) | `ArchiveError`; every refusal, failure and panic in one place | `ArchiveError` |
| [`internals`](internals.md) | The map of the source: layering, every file, the test infrastructure, what a change must keep true, and every public name in one doctest | — |

The rules of every split archive (the older `…SingleFile` format has no hash, no guard and no plan: [`single-file`](single-file.md)):

- Only a writer's `finish` returning `Ok` says everything written is in the working archive.
- A working archive opens only when its last flush completed and nothing was written to its files after it; otherwise `open` is `Err(Corrupted)`.
- A compaction is planned first — `compaction_plan()` gives the exact size of every file — and written from the plan with `write_compacted_to(&plan, &out)`: the data files first, the index last.
- Compact into an empty place. A compact archive is complete when its index exists AND opens.
- The same content compacts to the same bytes, as long as `max_data_file_bytes` is the same for every run. In a `fixed_key_minute_indexed_records` archive, records of an equal time keep the order they were written in, and that order is part of the content.
- A refusal — an `Err(ContractViolation)` decided before anything changes — leaves a writer usable. Any other `Err` of a writer, or a `write` / `flush` future dropped at an await, makes every later `write`, `flush` and `finish` an error.
- A failed read is an `Err`, never an absent key.
