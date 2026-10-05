# Code review — archived-data, 2026-10-05

What in the crate does not make sense: behaviour that contradicts a documented guarantee, APIs that cannot be
used as documented, checks that can never fire, comments that say something the code does not do, and tests
that do not test what they claim.

**Status, 2026-10-06: nothing listed here is fixed in the code.** The findings are open; `src/` changed only in
five doc comments that pointed at the old READMEs. The owner's decision so far: the compaction order of
[A1](#a1) stays as it is. A check of this report against the primary findings and the code made 61 corrections
across all of its sections and was stopped before it finished, so its last pass is missing.

## How it was done

- Ten reviewers, one per area: IO and local files, the split write path, the split read path and compaction,
  `fixed_key_preallocated_slots` with the SingleFile root files, `variable_key_preallocated_slots`, the records
  writer, the records reader and compaction, `global_day_index` with the small root files, the public API
  against the old READMEs, and the test suite.
- Every finding then went to a second agent whose job was to refute it. 71 findings: 55 confirmed, 13 confirmed
  in part (the claim was corrected), 3 refuted. Most behaviour claims were reproduced through the public API in
  a scratch crate, or by a mutation on a scratch copy of the crate. The repository was not modified.
- The entries marked **(docs pass)** were raised by the documentation writers and fact-checkers while they
  verified `docs/` against the code. Most were run, but they did not go through the separate refutation step,
  so no verifier set the severity such an entry carries.
- Identifiers such as `io-1` are the findings' ids. Duplicates across areas are merged; where their verifiers
  rated the same claim differently, the entry says so. Two low findings are not listed: `api-13`, a typo of the
  old root README that went with it, and `split-write-3`, a style difference no path reaches.
- B4, B10, B13, C5, D14 and the last two items of E8 carry neither an id nor the mark: they are from the
  reviewers' own notes — seen, not filed as findings — and went through no refutation step.
- "The docs" in an entry are what the reviewers were given: the doc comments in `src/` and the READMEs as
  committed — the root README before it became a map, and the two module READMEs the restructure deleted.
  `docs/*.md` was written from the code, and already states several of these behaviours as they are (A1, A2, A5,
  A10, A19, A20, A21, A22).

Baseline: 418 unit tests pass, `cargo build` has no warnings, `cargo clippy --all-targets` is clean.

## Summary

| # | Severity | What |
|---|---|---|
| [A1](#a1) | **high** | A compaction that fails in its data pass over a place that already holds another archive leaves the OLD index, which opens over the replaced data files and answers with other keys' items |
| [A2](#a2) | medium | Compacting into the directory the reader itself reads is not refused, and for keys written out of order returns `Ok(())` with other keys' items |
| [A3](#a3) | medium | The SingleFile readers hand out a cached L1 page without the span check: on a damaged file, `Ok(None)` for a key that is there |
| [A4](#a4) | low–medium | `PageCache::insert` never evicts at capacity 0: a SingleFile reader's `set_*_cache_size(0)` frees nothing. Medium by one verifier, low by the three who checked its duplicates |
| [A5](#a5) | medium | A records reader told a `day_start` off by less than a minute is not always refused, and `get_range` can skip records |
| [A6](#a6) | medium | `GlobalDayIndex::from_bytes` accepts structurally damaged files its doc says are `Corrupted` |
| [A7](#a7) | medium | `last_key_starting_with` is documented for `String` keys, which cannot hold its result |
| [A8](#a8) | medium | Test gaps on the crash-safety contract: 20+ single-line mutants of the readers and of the records writer leave all 418 tests green |
| [A9](#a9), [A15](#a15) | low–medium | SingleFile writers: a writer dropped after a flush leaves a file that opens, where the struct doc says "an unreadable file". `ArchiveFileWriter::finish`: an `Ok` that confirms nothing. Each is medium by one verifier, low by the others |
| [A10](#a10)–[A24](#a24) | low | The other fourteen smaller contradictions between a documented guarantee and the behaviour |
| [B](#b-api-and-design-questions) | — | API that cannot be used as documented, and design questions for the owner |
| [C](#c-checks-that-can-never-fire) | low | Checks that can never fire |
| [D](#d-comments-in-src-that-the-code-contradicts) | low | Comments in `src/` that the code contradicts |
| [E](#e-tests) | low–medium | Tests that cannot fail on what their name promises, and guarantees no test pins |
| [F](#f-cost-observations) | — | Cost observations, measured or read from the code |

## A. Behaviour against a documented guarantee

### A1

**A failed re-compaction leaves the old index over the new data files — high** (`io-1`, `api-1`; the verifier
of `fixed-4` rated it medium)

[reader_compact.rs:95-106](src/fixed_key_preallocated_slots/reader_compact.rs#L95-L106), the same in
[variable](src/variable_key_preallocated_slots/reader_compact.rs#L137-L150) and
[records](src/fixed_key_minute_indexed_records/reader_compact.rs#L97-L141).

`write_compacted_to` writes and finishes every data file and creates the index only afterwards. The docs derive
the completeness rule from that: "an `Err` in the data pass leaves no index", "a compact archive is complete
when its index exists AND opens", "running the compaction again replaces every file". All three hold only for
a destination that held no index. `LocalFileIo::create_*` replaces only the file it creates, so while the data
pass of a second run goes on, the index of the first run is still there and still verifies — the L0 hash covers
`size · next · count` only, nothing ties an index to its data files. If the second run fails, is dropped or
the process dies in its data pass, that index stays for good.

Reproduced by three agents through the public API over `LocalFileIo`:

- Archive A (200 keys, 4 data files) compacted into `dir`; archive B (A plus 30 lower keys) compacted into the
  same `dir` with `data01` failing to be created → `write_compacted_to` is `Err(Io)`, `Reader::open(dir)` is
  `Ok`, and 50 of A's 200 keys answer `Ok(Some(another key's items))`. With B's future dropped instead: 100 of
  200 keys silently wrong, no error anywhere.
- The records module opens the stale index just the same; there the walk's cell checks turn most reads into
  `Corrupted`, and only records of identical minutes and sizes come back wrong.
- Control: into an empty directory the failed run leaves no index, as documented. Re-running the SAME archive
  never answers with another key's items, the bytes are identical — but a run that died inside a data file
  left an archive that opens and answers `Corrupted` for some of its keys (80 keys: 76 right, 4 `Corrupted`
  at read).

This is the state a failed re-upload of a rebuilt day leaves in S3 too (RMS-847: data files first, index last —
the old index object names offsets inside the new data objects until the new index lands).

Fix: create the index writer and append its L0 frame BEFORE the data pass, and append the L1 pages after every
data file is finished — over `LocalFileIo` the old index is then replaced at once by one whose L1 pages lie past
its end, which `open` refuses. For factories that replace an object only on `finish` (S3), state the
precondition: the destination holds no index — delete it first, or write to a new prefix and switch. Add the
test: a run that fails over a complete archive must not leave something that opens.

Owner's note, 2026-10-06: a compact archive is built once, into an empty place, before it is uploaded to S3, so
the trigger does not occur in the archiver's flow, and the order of the passes stays. The precondition is
stated in [lifecycle](docs/lifecycle.md#compaction-the-data-files-first-the-index-last); the doc comments in
`src/` still state the rule without it.

### A2

**Compacting into the reader's own directory is not refused — medium, confirmed in part** (`api-2`)

[reader_compact.rs:97-101](src/fixed_key_preallocated_slots/reader_compact.rs#L97-L101),
[lazy_data_readers.rs:19-29](src/lazy_data_readers.rs#L19-L29).

Source data files are opened lazily, on first use, and `compaction_plan()` opens none. If `out` is the
directory being read, `LocalFileIo::create` removes `dataNN` before the reader has opened the source file of
that number; the reader then opens the half-written OUTPUT as its source. 40 keys written in descending order
over 10 data files, `write_compacted_to(&plan, &same_io)` → `Ok(())`; reopened: 20 keys right, 20 with another
key's items, no error. Ascending order and the records module happen to work, so a small test passes.

The readers do say "the archive is assumed immutable while it is read", so this is an unsupported use rather
than a broken promise — but nothing refuses it, and `LocalFileIo`'s "a reader opened on the old file keeps
reading the old bytes" reads as if it were safe. Fix: say in `write_compacted_to` and `LocalFileIo` that the
output must be another place; to make the mistake fail instead of corrupt, open every source data file an L1
page names before the first `create_data_writer`.

### A3

**SingleFile readers: a cached L1 page is not checked against the interval asking for it — medium**
(`fixed-1`, `variable-1`)

[fixed reader_single_file.rs:153-155](src/fixed_key_preallocated_slots/reader_single_file.rs#L153-L155),
[variable reader_single_file.rs:160-162](src/variable_key_preallocated_slots/reader_single_file.rs#L160-L162).

The span check (first key = interval's `from`, last = `to`) runs only on a cache miss. With a damaged L0 where
two intervals name one page, a cold reader answers `get` with `Corrupted`, and the same reader after it cached
that page for the other interval answers `Ok(None)` for a key that is in the file. The split readers close
exactly this hole (`split_l1_page` re-checks every hit, and a test pins it warm and cold); the SingleFile ones —
the readers of the objects already in S3 — do not, and have no such test. It takes a damaged file: no writer of
the crate produces two intervals with one offset. Fix: check every page `l1_page` returns, cached or just read.

### A4

**`PageCache::insert` returns before evicting at capacity 0 — low to medium** (`split-read-1` medium; the same
found as `api-5`, `fixed-6`, `variable-5` was rated low)

[page_cache.rs:55-57](src/page_cache.rs#L55-L57).

The SingleFile readers' `set_l1_cache_size` / `set_data_cache_size` only store the number; `insert` then
returns early at 0, before its eviction loop. Pages cached before a shrink to 0 stay in memory and keep
answering hits until the size is raised again, against "0 caches nothing. A shrink evicts at the next insert".
A shrink to any other size does evict. Measured with a counting source: keys read cold cost 30 reads, after
both caches were set to 0 and 40 `get`s of other keys went by, the same keys cost 0 reads. Every answer stays
right — the file is immutable — so what is lost is memory, at most the pages the old size allowed.

There are two resize mechanisms for one cache: the split, records and day-index readers use `resize` (evicts at
once, hence `async`) and `insert_within_capacity`; the SingleFile readers kept `set_capacity` and `insert`,
which also reads the capacity before taking the lock. That is the only reason one setter is `async` and the
other is not. Fix: one mechanism for every reader, or at least `evict_down_to(pages, 0)` before the early
return.

### A5

**A records reader told a slightly wrong day is not always refused — medium** (`records-read-1`)

[reader.rs:145](src/fixed_key_minute_indexed_records/reader.rs#L145),
[writer.rs:81-108](src/fixed_key_minute_indexed_records/writer.rs#L81-L108).

The docs promise that a reader told the wrong day answers `Corrupted` for a read that meets a record. A shift
of a whole minute or more is always caught: the first record met lies in another minute than its cell, or
outside the day. A non-zero shift under a minute can escape both day checks, because most records keep their
minute. Measured: records at 10:20, 10:55, 11:05, 11:30 (minute:second), `get_range(10:50 ..= 11:10)` gives
`[2, 3]` on the right day and `Ok([3])` from a reader opened 15 seconds early — record 2 is silently missing.
A compaction by a reader opened 15 seconds late succeeds and writes the cells under the wrong minutes; a
right-day reader of that compact archive then gets `minutes_with_records` `[10, 15]` instead of `[10, 11, 15]`
and `Corrupted` from `get_day`. Nothing requires an aligned `day_start`: an archive can be written with
`day_start = DAY + 15 s`.

Fix: refuse a `day_start` that is not a whole minute in `Writer::new_with_capacity` and `Reader::open`. With
whole-minute starts on both sides every mismatch is at least a minute and always refused at the first record
met.

### A6

**`GlobalDayIndex::from_bytes` / `read_from`: "damaged anywhere is `Corrupted`" is not true — medium**
(`day-index-2`, `split-read-3`)

[index.rs:44-63](src/global_day_index/index.rs#L44-L63),
[reader.rs:57-59](src/global_day_index/reader.rs#L57-L59).

Each L1 page and each login's blocks are checked, the layout of the data area is not: nothing ties a
`data_offset` to the position the format defines, checks that two logins do not share blocks, or that the file
ends at the last block. Measured: login 4's `data_offset` overwritten with login 3's → `from_bytes` is `Ok` and
login 4 reports login 3's days; `to_bytes` then writes that out as a clean file. 1000 trailing bytes are
accepted; 4 of the 32 single-bit flips of a `data_offset` are accepted and replace a login's days with garbage.
And `GlobalDayIndexReader::open` reuses the split index's "cut inside the last L1 page" check although this file
ends with its year blocks: a download cut inside the year blocks opens, and only lookups of logins past the cut
fail (`from_bytes` does refuse that file, it reads every block).

It matters because the archiver's recipe builds the next uploaded version from `from_bytes` of a downloaded
copy. Fix: in `read_from` track the expected data offset (it starts at the end of the last L1 page and advances
by `blocks × 48`) and require the file to end there; in `open`, check the last login's
`data_offset + blocks × 48` against the file size. This also replaces the dead check in [C1](#c-checks-that-can-never-fire).
The doc sentence needs narrowing either way: a flipped day bit passes, and no check can see it without a
checksum.

### A7

**`last_key_starting_with` is documented for `String` keys — medium** (`api-3`, `variable-4`)

[key_starting_with.rs:6-11](src/variable_key_preallocated_slots/key_starting_with.rs#L6-L11).

The bound is the prefix followed by `0xFF` up to 32 767 bytes, and `get_range` takes it as `&TPrimaryKey`.
`0xFF` never occurs in UTF-8, so a key that keeps a `String` cannot hold it: `from_slice` with
`from_utf8().unwrap()` panics, and with `from_utf8_lossy` the bound becomes `EF BF BD` repeated, which sorts
BELOW every 4-byte character and `U+FFFF` — measured, 3 of 9 keys under `node/` are silently missing from the
range. "A byte slice" cannot implement the trait at all (`from_slice` returns an owned `Self`), and `Vec<u8>`
cannot implement it outside the crate (orphan rule), so the documented call type-checks only through a newtype
over bytes. Names and paths are the module's stated use, so a `String` key is the natural reader of this doc.

Related **(docs pass)**: `PrimaryKeyVariableSize::from_slice` returns `Self`, not a `Result`, so a key that
keeps a `String` can only panic on a damaged index, where everything else in the crate answers `Corrupted`.

Fix: limit the doc to keys that keep raw bytes and show the conversion through `from_slice`; offer a bound for
UTF-8 keys (the prefix followed by `U+10FFFF` up to the length limit) if `String` keys are meant to be
supported.

### A8

**Test gaps on the crash-safety contract — medium** (`tests-2`, `tests-3`, `tests-4`, `tests-5`,
`records-read-2`)

See [E](#e-tests): the records writer has its own `write` / `flush` / `finish` and no dropped-future,
failed-flush or failed-finish test; "a failed read is never an absent key" is tested only with the first read
failing; the corruption matrix of the split readers passes when `get` answers `None` for a damaged L1 page.
Each is a gap, not a bug — today's code behaves as documented in every probe — but more than twenty
single-line mutants that break exactly the documented guarantees leave all 418 tests green.

### A9

**SingleFile writers: the doc promises an unreadable file, and a failed flush can be "retried" into a lost
item — low to medium** (`variable-2` medium; `api-4`, `fixed-3`, `fixed-7` low)

- [fixed writer_single_file.rs:25-28](src/fixed_key_preallocated_slots/writer_single_file.rs#L25-L28),
  [variable :62-63](src/variable_key_preallocated_slots/writer_single_file.rs#L62-L63): "dropping the writer
  without one leaves an unreadable file" is true only before the first flush. After a completed flush a dropped
  writer leaves a file that opens and may answer what no flush saved (measured: 8 keys flushed, 33 more writes,
  drop → opens, `keys_count` 6, 4 of the 8 flushed keys are `Err`, an unflushed item is answered). The module
  READMEs said so; the struct docs leave it out. "Every `Err` leaves the file in an unknown state" is also
  too wide: a slot or an item out of contract is refused before anything changes. Three verifiers judged the
  first sentence and split: `variable-2` kept it at medium — a reader of the rustdoc can take "it opens" for
  "it is complete", or a flush for a durable checkpoint; `api-4` and `fixed-3` lowered it — the sentence is true
  of a writer that never completed a flush, the usual write … `finish` flow, and the working file is a
  temporary that is rebuilt.
- [writer_data_pages.rs:140-141](src/writer_data_pages.rs#L140-L141) clears `dirty` BEFORE the awaited
  `write_at`; the L1 store and the split data store clear it after. A flush that fails there, flush again →
  `Ok`, `finish` → `Ok`, and the item is not in the file — the struct doc says the writer is not usable after
  an `Err`, but nothing enforces it. With the flush future dropped halfway and `finish` called: 299 of 300 files
  had a data page of zeros **(docs pass)**.
- The SingleFile writers have no in-flight guard, so a dropped `write` / `flush` is not detected;
  `PositionedSink` records its position before `write_all` finishes
  ([positioned_sink.rs:22-32](src/positioned_sink.rs#L22-L32)), so a later write at that offset skips its seek
  **(docs pass)**.

### A10

**"A refusal changes nothing" — a refused new key still evicts the caches — low** (docs pass; noted by the
split-write, variable and records reviewers too)

[fixed writer.rs:175-183](src/fixed_key_preallocated_slots/writer.rs#L175-L183),
[variable writer.rs:242-246](src/variable_key_preallocated_slots/writer.rs#L242-L246),
[records writer.rs:175-179](src/fixed_key_minute_indexed_records/writer.rs#L175-L179).

A placement refusal reads the key's L1 page in and evicts the caches down to their size before it returns. That
can write a dirty page an EARLIER write left, with the spoil of frame 0's hash: a working archive that opened
stops opening until the next flush (measured: one index read, two index `write_at`s, `open` from "3 keys" to
`Corrupted`). And when that eviction fails, the caller gets `Err(Io)` and a stopped writer instead of the
refusal. Nothing of the refused key is written, and the writer's own state is unchanged, but "nothing" is not
"no write". Dropping only the clean pages the call read in would keep the promise.

### A11

**A broken `Default` item stops the writer instead of being refused — low** (docs pass)

[split_writer_data_pages.rs:61-65](src/split_writer_data_pages.rs#L61-L65).

`TItem::default()`'s byte count is checked only when the first data page is built, after `reserve_page` created
data file 0 and reserved the page. A `Default` whose `copy_to` is off gives `ContractViolation` with exactly
the message a harmless refusal gives, leaves a reserved page that is not cached, and every later call is "the
writer is not usable". It also leaves `keys == 0` with a data file, which makes `set_max_data_file_bytes`
panic with a message its doc does not list — "the data file size cannot change once a data page is placed";
the doc lists above 4 GiB, below one data page, and "once a key is written". Building the empty page in
`new_with_capacity` (already fallible) would make it a clean construction error.

### A12

**The `copy_to` length check underflows — low, confirmed in part** (`fixed-2`)

[primary_key.rs:22-24](src/fixed_key_preallocated_slots/primary_key.rs#L22-L24),
[preallocated_data_item.rs:22-24](src/preallocated_data_item.rs#L22-L24).

`out.len() - before` is meaningful only if `copy_to` appends. With an implementation that replaces the buffer
(`*out = bytes.to_vec()`) it underflows once the buffer already holds more than `LEN` / `ITEM_LEN` bytes: a
panic, "attempt to subtract with overflow", in a debug build — where the docs promise a `ContractViolation` —
and in release a `ContractViolation` with a nonsense count, "wrote 18446744073709551611 bytes". The refusal
probe encodes the key into an EMPTY Vec, where replacing and appending look the same, so for a key the breach
shows only at eviction or flush, as a failure that stops the writer, not as a refusal. Always loud, never a
corrupted archive, and only for an implementation that breaks "appends". Fix: call `copy_to` on a scratch Vec,
check its length, then extend `out`.

### A13

**The fixed SingleFile writer never checks the key's byte count — low** (docs pass)

[writer_single_file.rs:103-122](src/fixed_key_preallocated_slots/writer_single_file.rs#L103-L122). A key whose
`copy_to` writes 7 bytes is accepted by `write`, and every later flush fails. The split writer refuses it up
front ([writer.rs:284](src/fixed_key_preallocated_slots/writer.rs#L284)) and so does the variable SingleFile
writer. The two split writers also check the key at different moments: the fixed one only for a new key, after
its L1 page was read; the variable one on every write, before any read.

### A14

**Records writer: a damaged frame panics mid-insert instead of `Corrupted` — low** (`records-write-1`)

[records_frame.rs:28](src/fixed_key_minute_indexed_records/records_frame.rs#L28) accepts any record length that
ends inside the frame; [:129](src/fixed_key_minute_indexed_records/records_frame.rs#L129) asserts the insert
fits. A working data file whose length prefix was damaged into a record longer than `MAX_RECORD_LEN` can make
`write` panic after `split_off` already moved the upper half — a third panic the writer's doc does not name; a
small record into the same frame is written with no complaint. The writer clearly means `Corrupted` here: a
full frame that reads as ONE oversized record is refused with "is full with one record". Fix: refuse
`len > MAX_RECORD_LEN` in `frame_record`.

### A15

**`ArchiveFileWriter::finish`: an `Ok` that confirms nothing — low to medium** (`tests-1` medium; `io-4`,
`io-8`, `io-3` low)

[local_file_io/writer.rs:162-209](src/local_file_io/writer.rs#L162-L209),
[archive_file_writer.rs:10](src/archive_file_writer.rs#L10) and
[:27-37](src/archive_file_writer.rs#L27-L37).

- The `Inner` is taken before the outcome is known. A `finish` that failed, retried → `Ok(())`, with nothing
  synced. `let f = w.finish(); drop(w); f.await` → `Ok(())` at once (500 ns against 4.5–9 ms), while `Drop`'s
  sync runs detached and its failure only panics on a blocking thread. A second `finish` awaited while the
  first is still syncing → `Ok(())` at once. Two tests pin this on purpose. It contradicts "`finish` is where
  the file is confirmed, and an `Err` from it means the file is not there" in one legal call order of a
  `'static` future — and the trait doc hands that shape to S3 implementers, where a lost upload failure matters
  more than a lost fsync. No caller inside the crate does it. That a second call is `Ok` is documented; that
  such an `Ok` confirms nothing is not. The verifier of `tests-1` rated this medium for the S3 reason; the
  verifier of `io-4` rated the same three cases low, because nothing inside the crate is affected.
- The doc gives the reason for the `'static` future as "`Drop` hands it to `tokio::spawn`". `LocalFileWriter`'s
  `Drop` does not: it runs the sync through `spawn_blocking`, or on a thread of its own, precisely because a
  writer can outlive its runtime. A writer built to the doc's letter panics when dropped outside a runtime.
- `Drop` during a runtime shutdown calls `spawn_blocking` and ignores the outcome: tokio can drop the task, and
  then the sync never runs, with no panic and no error (a race: skipped in 29 of 200 runs with `#[tokio::main]`
  defaults on 10 cores, in 200 of 200 with one or two worker threads, in 75 of 100 with an idle task, on
  current-thread runtimes too). Only the fsync of an abandoned file is lost.
- **(docs pass)** `finish` polled outside a runtime takes the `Inner` and then panics in `spawn_blocking`: the
  file is closed unsynced and `Drop` finds nothing. `Drop` is runtime-proof, `finish` is not.

Fix: keep the outcome in the slot (`Open` / `Finishing` / `Done(result)`), so every `finish` returns the one
real result and a `finish` awaited after the drop waits for it; run the unfinished-writer `Drop` on a thread
always.

### A16

**The two readers of the sync time disagree — low** (`api-6`, `day-index-1`, `io-5`, `day-index-4`)

[reader.rs:71-74](src/global_day_index/reader.rs#L71-L74), [index.rs:73-79](src/global_day_index/index.rs#L73-L79).

For an object shorter than 8 bytes, `GlobalDayIndex::sync_time_of`, `from_bytes` and `open` answer `Corrupted`,
and `read_sync_time` answers `Io(UnexpectedEof)` — the one reader-side `read_at` that bypasses `SizedReader`.
Both functions sit in impls generic over a `TKey` they never use, so the calls the docs name do not compile
without a turbofish (`E0283`). Fix: map the short read to `Corrupted`; make the two functions free functions.

### A17

**SingleFile: a file cut short is `Io` or `Corrupted` depending on where the cut falls — low** (`io-2`,
`fixed-5`)

[source_io.rs:55-65](src/source_io.rs#L55-L65), [data_page.rs:117-119](src/data_page.rs#L117-L119).

A cut L0 frame → `Corrupted` at open; a cut L1 header → `Corrupted` from `get` and `Io(UnexpectedEof)` from
`keys_count`; a cut L1 body or data page → `Io("early eof")` with no offset. The split readers answer
`Corrupted` for every cut. `check_data_page`'s two "cut short" branches can never fire: its one caller always
passes a buffer of the exact size. Nothing promises `Corrupted` here — the SingleFile promise is "an `Err`,
never `None`" — so it is an inconsistency, not a broken promise. **(docs pass)** The SingleFile readers also
allocate whatever size a damaged header claims, up to 4 GiB, before they learn the file is shorter.

### A18

**No reader checks how many bytes `read_at` returned — low** (docs pass)

[sized_reader.rs:22-47](src/sized_reader.rs#L22-L47), [global_day_index/reader.rs:152-155](src/global_day_index/reader.rs#L152-L155).

The contract says exactly `len` bytes or `UnexpectedEof`. A reader that breaks it — a raw HTTP ranged GET past
the end returns a short body, the natural mistake of an S3 implementer — makes the decoders slice out of
bounds: `get` panicked with "range end index 9984 out of range for slice of length 9981" in `le_bytes.rs`,
`read_sync_time` panics on `bytes[..8]`, and `decode_year_blocks` over zero bytes returns no block, so
`days_with_data` answers `Ok([])` for a login that has days. The writer-side decoders do check. One
`bytes.len() != len` check in `SizedReader` and in `read_sync_time` would turn all of these into errors.

### A19

**A smaller writer data cache takes effect only at the next new key — low** (`split-write-1`)

[split_writer_data_pages.rs:39-42](src/split_writer_data_pages.rs#L39-L42). `set_data_cache_size` only stores
the number, and the data cache is trimmed in `preallocate` alone. After `set_data_cache_size(0)`, ten items to
known keys reach no file before the next flush — against "writes every item straight to its file". The L1
cache and the records writer trim after every write; the split readers' setters evict before they return. The
SingleFile writer goes further at 0: its `preallocate` returns before evicting
([writer_data_pages.rs:55-57](src/writer_data_pages.rs#L55-L57)), so pages cached before a shrink to 0 are
never trimmed, not even by a new key **(docs pass)**.

### A20

**Variable keys: one long key can halve the usable index — low** (`variable-3`)

[split_layout.rs:121-131](src/variable_key_preallocated_slots/split_layout.rs#L121-L131),
[writer.rs:424-427](src/variable_key_preallocated_slots/writer.rs#L424-L427).

The L0 continuation bound is a valid upper bound, so a flush never runs into 4 GiB. Its cost is not what the
comments say ("where one is loose the other is not", "such a key costs a frame or so"). With the L0 frame at
the minimum the constructor accepts (16 386 bytes for 16 KiB pages), after ONE 8 179-byte key the writer
refuses new keys at 131 064 L1 pages — 50.0% of 4 GiB — against 255 749 without it. With the default 64 KiB
frame the loss is under 1%. The existing test passes only because its frame is four times the page. Cheapest
fix: require room for two largest intervals at construction; or a tighter bound.

### A21

**"Any write order compacts to the same bytes" is false for records of an equal time — low**
(`records-read-5`)

The records writer keeps records of an equal time in the order they were written, and compaction copies that
order. Two records of one key at the same microsecond, written in the two orders: identical index, DIFFERENT
data file. The determinism test steps around it ("every time distinct"). The claim was the old root README's,
whose archiver recipe built "a day rebuilt from the same rows uploads identical bytes" on it while recommending
`ORDER BY login, time`, which leaves rows of one login with one timestamp in no defined order — a realistic
case for deals. A rebuild needs a total order: login, time, a tie-breaker. `docs/lifecycle.md` now states the
condition and `docs/archiving-a-day.md` the tie-breaker; the doc comment of the records `write_compacted_to`
still reads "a function of the keys, the records and the plan's `max_data_file_bytes` only"
([reader_compact.rs:72-73](src/fixed_key_minute_indexed_records/reader_compact.rs#L72-L73)).

### A22

**"A compact key of a few frames is one read" is false — low** (`records-read-3`)

[records_read_ahead.rs:14-15](src/fixed_key_minute_indexed_records/records_read_ahead.rs#L14-L15). A cold
walk's first read is always one frame, and a compact frame 1 exists only because it did not fit the first
32 KiB. Measured on a cold reader, with frames of four 8 000-byte records: 1 frame = 1 read, 2–3 frames = 2,
4–7 = 3, 8 and 15 = 4, 16 = 5; from four frames on these are upper bounds **(docs pass)**. The other figure
holds: 17 000 records = 173 frames = 14 reads. The crate's own test asserts 3 reads for 5 frames.

### A23

**A plan of another key length writes files before it is refused — low** (docs pass)

`write_compacted_to` given a `CompactionPlan` of fixed keys of another `LEN` writes `data00` and a wrong, short
`index` before it answers `Corrupted`. The `ContractViolation` in
[compact_index_writer.rs:18-31](src/compact_index_writer.rs#L18-L31) ("a plan of another key type") cannot
fire for a plan of the same module. A key-length field in the plan, checked first as the records plan does,
would refuse before anything is created.

### A24

**Construction checks multiply unchecked — low** (docs pass)

[data_page.rs:17-19](src/data_page.rs#L17-L19) computes `14 + SLOTS × ITEM_LEN` unchecked where
`split_data_page.rs` uses `checked_mul`; the L0 / L1 size checks multiply `capacity × entry_len` the same way.
An absurd `SLOTS` or capacity panics with an arithmetic-overflow message in debug and wraps past the check in
release.

## B. API and design questions

| # | What | Where |
|---|---|---|
| B1 | `LocalFileReader` / `LocalFileWriter` are public but cannot be constructed: `new` is `pub(crate)`, the only public source is `LocalFileIo`, which opens exactly `<dir>/index` and `<dir>/dataNN`. `GlobalDayIndexReader` is documented as reading "a local file" and takes ONE reader; the archiver names that file itself. A local copy can be read "where it lies" through the crate's own reader only if it is called `index` in a directory of its own; otherwise the caller loads it whole with `from_bytes` or brings a reader of its own. A public `LocalFileReader::open(path)` is missing (`api-7`, `io-6`, `day-index-7`) | [reader.rs:19](src/local_file_io/reader.rs#L19) |
| B2 | `ArchiveFileWriter::size` is a required trait method no non-test code calls: every writer counts with `AppendFrontier`. With it deleted from the trait and from `LocalFileWriter`, the library builds without an error or a warning. An S3 writer must implement something nobody asks (`api-8`, `io-7`) | [archive_file_writer.rs:51](src/archive_file_writer.rs#L51) |
| B3 | `ContractViolation` means four things: a broken trait contract, a limit (index 4 GiB, data file 256), a refusal that leaves the writer usable, and the answer of a poisoned writer. No `is_usable()`, so a caller that skips refused rows can tell "go on" from "rebuild" only by the message. The preallocated writers check the slot and the item — the variable one the key too — BEFORE the in-flight guard, so a poisoned writer answers a bad slot with the harmless-looking refusal; the records writer checks the guard first (`api-11`, `io-9`) | [in_flight_guard.rs:20-27](src/in_flight_guard.rs#L20-L27) |
| B4 | `DateTimeAsMicroseconds` is in the public API (records writer / reader, day index) but `rust_extensions` is not re-exported: a consumer must depend on the same git tag, or the types differ | [lib.rs](src/lib.rs) |
| B5 | **For the owner to confirm.** A WORKING records archive is one data file of 4 GiB in 32 KiB frames: at most 131 072 frames a day, so at most 131 072 keys, fewer with busy accounts — and crossing it is a PANIC (documented, required by the spec). Running out of index room is a polite refusal that, with the default capacities, can never be reached first. The spec sizes the global day index for 200 000 accounts (docs pass) | [writer.rs:286-296](src/fixed_key_minute_indexed_records/writer.rs#L286-L296) |
| B6 | The two panics of the records `write` are inconsistent: the day check runs before `in_flight.begin()`, `check_data_room` after it — a caught panic leaves the writer usable in one case and unusable in the other. The construction panics carry no archive name, against "called `name` in every panic" (docs pass) | [writer.rs:164-166](src/fixed_key_minute_indexed_records/writer.rs#L164-L166) |
| B7 | A reader's `set_max_data_file_bytes` is a per-plan parameter kept as shared mutable state: an `AtomicU64` on a reader shared through an `Arc`, read at the END of `compaction_plan`. Two tasks planning with different sizes race. `compaction_plan(max_data_file_bytes)` would say what it is (docs pass) | [reader.rs:94-101](src/fixed_key_preallocated_slots/reader.rs#L94-L101) |
| B8 | `get(key, from, to)` with `from > to` answers `Ok(Some(vec![]))` before the key is looked up — also for a key the archive does not hold — while `get_range` lists exactly the keys that exist. Tested, so deliberate, but `Some` for an absent key is a trap for `is_some()` (docs pass) | [split_reader_pages.rs:32-37](src/split_reader_pages.rs#L32-L37) |
| B9 | The factory traits take the file type as a type PARAMETER where an associated type would do: that is why the preallocated split writers have six type parameters and their readers are opened as `::<Login, Hour, Volume, _, _>` (docs pass) | [io_writer_factory.rs:16](src/io_writer_factory.rs#L16) |
| B10 | Day numbering in the global day index is the UTC calendar day of the microsecond value. If a day archive's `day_start` were a true UTC instant of a midnight in a zone ahead of UTC, `set_day(day_start)` would mark the previous day. Consistent if server-clock times are naive microseconds. `docs/global-day-index.md` describes the numbering and a `day_start` that is not its midnight; no doc says the times are naive | [calendar.rs](src/calendar.rs) |
| B11 | Unverified, read from the code: on Windows `create_*` = `remove_file` + `create_new(true)` presumably FAILS while a reader still holds the old file open. The crate's test asserts the old-reader behaviour under `#[cfg(unix)]` only (docs pass) | [factory.rs:62-73](src/local_file_io/factory.rs#L62-L73) |
| B12 | `write_pascal_string` returns the length kept — the only sign that a string was cut — and is not `#[must_use]`. `ArchiveError`'s `Display` prints the inner `io::Error` AND `source()` returns it, so error-chain reports show the message twice (docs pass) | [pascal_string.rs:14](src/pascal_string.rs#L14), [error.rs:13-27](src/error.rs#L13-L27) |
| B13 | Dangling references for a crate meant to leave the monorepo: the SingleFile docs explain cancel-safety through `S3Reader`, a type that is not in this crate; the records and day-index docs point at a Task manager document of project RMS as the byte-layout spec | several |

## C. Checks that can never fire

| # | What | Where |
|---|---|---|
| C1 | "the L1 page repeats a login of another page": L0 intervals are verified ordered and disjoint, and every page spans its interval with strictly ascending logins. Seven patched files were all refused earlier (`day-index-3`) | [index.rs:56-61](src/global_day_index/index.rs#L56-L61) |
| C2 | The compaction writers' "a file that ends at a size other than planned" checks, and `reserve` followed at once by `check_append`: the count, length and bound checks before them already fix every size. 273 000 simulated layouts, never reached. The docs present it as a separate failure mode (`split-read-2`) | [compact_data_files_writer.rs:71-99](src/compact_data_files_writer.rs#L71-L99), [compact_index_writer.rs:70-79](src/compact_index_writer.rs#L70-L79) |
| C3 | `refuse_unordered` in `keys()` and in every `compaction_plan()`: `open` plus the page decode already guarantee key order across pages. The message "keys do not ascend across L1 pages" is never produced (`records-read-4`) | [reader_keys.rs:24](src/fixed_key_minute_indexed_records/reader_keys.rs#L24), [reader_compact.rs:35](src/fixed_key_minute_indexed_records/reader_compact.rs#L35) |
| C4 | `check_data_page`'s "cut short" branches (see A17) | [data_page.rs:86-96](src/data_page.rs#L86-L96) |
| C5 | Smaller: `.unwrap_or(0)` at [reader.rs:145](src/fixed_key_minute_indexed_records/reader.rs#L145); `within.min(window.len())` at [records_read_ahead.rs:107](src/fixed_key_minute_indexed_records/records_read_ahead.rs#L107); the final size check of [compact_records_index.rs:64-70](src/fixed_key_minute_indexed_records/compact_records_index.rs#L64-L70), which would also report an internal bug as `ContractViolation`; the start-minute check at [records_cursor.rs:170](src/fixed_key_minute_indexed_records/records_cursor.rs#L170) — redundant rather than dead: it does fire, and the cell check at :188 would catch the same cases, all but two cells sharing one pointer | — |

`ArchiveFileWriter::size` (B2) belongs here too.

## D. Comments in `src/` that the code contradicts

| # | The comment says | The code does | Where |
|---|---|---|---|
| D1 | "`hash` is FNV-1a 64 of the L0 header's three fields" | It hashes `archived-data/split/L0/v1` first, then the fields. A hash computed as written verifies against no index (`api-12`, `fixed-10`) | [fixed mod.rs:17](src/fixed_key_preallocated_slots/mod.rs#L17), [variable mod.rs:18](src/variable_key_preallocated_slots/mod.rs#L18) |
| D2 | "every module of this crate shares" the data page and the L0 chain framing | Only the four `…SingleFile` structures use them; the split structures use `split_data_page.rs` / `split_l0_chain.rs` (`fixed-9`, `api-14`) | [data_page.rs:1](src/data_page.rs#L1), [l0_chain.rs:1](src/l0_chain.rs#L1), [lib.rs:5-6](src/lib.rs#L5-L6) |
| D3 | The working flush is "the same for every module" | Only the two preallocated writers call it. The records writer carries its own copy of the flush and of `finish_split_writers` — a maintenance trap around the frame-0 protocol; the copies already differ (the shared path encodes L0 before the early return) (`split-write-2`) | [split_working_flush.rs:7](src/split_working_flush.rs#L7), [records writer.rs:202-264](src/fixed_key_minute_indexed_records/writer.rs#L202-L264) |
| D4 | The SingleFile structures "read and write one file through `AsyncRead + AsyncSeek`" | The writers need `AsyncWrite` too (`day-index-6`) | [lib.rs:10-11](src/lib.rs#L10-L11), both `mod.rs` |
| D5 | "a continuation once reserved is rewritten in place by every later flush" | `lay_out` writes only the frames it needs; with variable keys a flush may link fewer, and the unlinked ones keep stale bytes (`split-write-4`) | [split_l0_chain.rs:43-44](src/split_l0_chain.rs#L43-L44), [l0_chain.rs:144-145](src/l0_chain.rs#L144-L145) |
| D6 | "a cut can only fall in the page at the highest offset" | False for two of the helper's four callers: a WORKING records index ends with L2 frames, the global day index with year blocks (`split-read-3`) | [split_reader_index.rs:39-41](src/split_reader_index.rs#L39-L41) |
| D7 | "A key written before is never lost to a write that fails or is dropped" | True only of the poisoned writer's memory, which only a `#[cfg(test)]` hook can persist. After a failure `finish` is refused and every key since the last completed flush is gone (`fixed-8`) | [writer.rs:45-47](src/fixed_key_preallocated_slots/writer.rs#L45-L47) |
| D8 | The in-flight mark is "cleared … on that call's successful return ONLY" | A refusal clears it too — that is why the writer stays usable (docs pass) | [in_flight_guard.rs:5-6](src/in_flight_guard.rs#L5-L6) |
| D9 | "a record after the last one of a full frame goes into a new frame chained after it" | Only for the chain's LAST frame; elsewhere the record is placed at the start of the next frame (docs pass) | [records writer.rs:22-23](src/fixed_key_minute_indexed_records/writer.rs#L22-L23) |
| D10 | An error message, not a comment: "the index points at a data frame at X" | Also when the bad offset came from a data frame's `next`; `load`'s own doc comment is neutral (`records-write-2`) | [writer_data_frames.rs:102](src/fixed_key_minute_indexed_records/writer_data_frames.rs#L102) |
| D11 | The `ArchiveError` variants: "a trait implementation broke its contract", "the sink or the source failed" | `ContractViolation` also covers limits, plan sizes and a poisoned writer; `Io` also a factory that cannot create or open a file (`io-9`) | [error.rs:5-10](src/error.rs#L5-L10) |
| D12 | A constant, not a comment: the records type-size check pads with `DEFAULT_L1_CAPACITY` | The compact page it covers, and every sibling check, use `COMPACT_L1_CAPACITY`; both are 1024, no effect today. `records_compaction_plan.rs:40` sizes an L0 entry with `L2_OFFSET_LEN`, the wrong field's constant, same value (`records-write-3`, `api-10`) | [layout.rs:55](src/fixed_key_minute_indexed_records/layout.rs#L55) |
| D13 | Memory of a compaction: "one L1 page plus one data page" | The index pass holds a source L1 page plus the compact page it builds; the variable SingleFile stream holds the page plan; the records walk its read-ahead window (docs pass) | `reader_compact.rs`, `reader_single_file_compact.rs` |
| D14 | "Below `max_data_file_bytes - page_len`"; `MAX_SPLIT_DATA_RUN_BYTES` limits "a run of data pages"; `SplitFrameHeader.field` is `item_len` | "at most"; it limits the L1 runs too; the records module reads the same `u16` as `used` | [data_file_layout.rs:49](src/data_file_layout.rs#L49), [split_data_runs.rs:10](src/split_data_runs.rs#L10), [split_frame.rs](src/split_frame.rs) |

The doc comments that pointed at the READMEs (`lib.rs`, four `mod.rs`) were updated with the docs restructure.

## E. Tests

E1–E6 were shown by mutation on a scratch copy: one line changed, the whole unit suite run. E7 was shown by
probes and by recomputing, E8 by searching the tests.

| # | What | Where |
|---|---|---|
| E1 | `corrupted_headers_chains_and_refs_are_errors_never_none` passes when `get` answers `None` for a damaged L1 page: its helper accepts every `Ok(_)` from `get`, and the name of the call that noticed is computed and never compared. With `Corrupted → Ok(None)` in both split readers' `data_ref_of`, 415 of 418 tests pass, both table tests among them (`tests-5`) | [tests_split_reader.rs:407](src/fixed_key_preallocated_slots/tests_split_reader.rs#L407) |
| E2 | The records writer has its own `write`, `flush_in_flight`, `finish` and page stores, and one failure injection in six test files. Mutants that survive: a dropped `write` / `flush` no longer poisons the writer; frame 0 written with its real hash BEFORE the data frames; no spoil before an appended data frame; `finish` skipping the data file after a failed index finish; the finish error returned before the flush error; `finish` never finishing the data file. The same order mutation in the shared flush is killed by 8 tests (`tests-2`) | [tests_working_index.rs:141](src/fixed_key_minute_indexed_records/tests_working_index.rs#L141) |
| E3 | "A failed read is never an absent key / never a partial `Ok`" is tested only with the only data file failing from its first read. `fail_reads_after`, built for exactly this, is called by nothing but its own test. Mutants that survive: `get` answering `Ok(None)` when the read of the key's L1 page fails; `get_range` ending with `Ok` when a LATER run fails; `keys_count` skipping an unreadable L1 page (`tests-3`) | [tests_split_range.rs:427](src/fixed_key_preallocated_slots/tests_split_range.rs#L427) |
| E4 | No records reader or compaction test makes a read, an append or a finish fail. Mutants that survive: `get_day` answering the records found so far when a frame read fails; a failed L1 / L2 read answering an empty list; `keys()` ending at an unreadable page; `write_compacted_to` ignoring the data pass's final close and writing the index (`tests-4`) | [tests_reader.rs:239](src/fixed_key_minute_indexed_records/tests_reader.rs#L239) |
| E5 | The cycle, time-order and cell-past-records checks of the records walk are not pinned: each alone removed leaves 418 passing. Without `visited`, an equal-time frame chained to itself makes `compaction_plan` never return; without the first-frame check, a cell past the records gives `get_day = Ok([])` for a key that holds 2 records (`records-read-2`) | [records_cursor.rs:108-177](src/fixed_key_minute_indexed_records/records_cursor.rs#L108-L177) |
| E6 | Not pinned: the two edge rules of records decision 3 (a place exactly at the halving boundary, a tie between two boundaries), decision 7 for a non-first frame, the writer's cycle and read-back checks of decision 6 (`tests-8`) | [tests_overflow.rs:246](src/fixed_key_minute_indexed_records/tests_overflow.rs#L246) |
| E7 | The case "laying the L0 chain out" drops the flush at the spoil of frame 0's hash, before the data pages are stored, so it never reaches the L0 lay-out (`tests-6`). Test helpers of the variable module derive a key's value from the SUM of its bytes or its length: `numbered_keys(40)` has 13 distinct records, so the read-backs cannot tell one key's page from another's (`tests-7`). `is_read_waiting` / `is_write_waiting` stay true for ever once a parked call is dropped (`tests-9`). Six comments give decimal megabytes as MiB (`tests-10`) | — |
| E8 | No test at all for: a compaction that fails over an existing archive (A1); a shrink to 0 through `set_capacity` + `insert` (A4); the cut-back of a failed local append; an item whose `copy_to` writes a wrong byte count | — |

## F. Cost observations

Raised by the documentation fact-checkers — the bulk shrink of F5 and the last sentence of F9 by reviewers'
notes; not defects. F1, F2, F6 and the reads of F9 were measured; the rest is read from the code, its figures
computed.

| # | What |
|---|---|
| F1 | The variable split writer's page split keeps the left page FULL and moves only the overflow (`split_layout.rs:31` calls it "a halving by bytes"). 400 000 random 8-byte keys: 1.73× the index IO of the fixed writer and about 9× the wall time in memory (20.0 s against 2.2 s). Cutting nearest the byte midpoint among the cuts where both parts fit keeps the half-page proof |
| F2 | The variable SingleFile writer has no "into the next page" rule: any key landing in a full page costs a whole new 16 KiB page. 2000 keys of 20 bytes written descending → 1437 L1 pages, 22 MiB of L1 for 58 KB of entries |
| F3 | Compaction — the heaviest full scan — is the one path that does not use the run reads: every data page has its own `read_at`, in key order, and the source's L1 pages are read three times uncached. Re-compacting a compact archive from S3, the verification the docs recommend, is one ranged GET per key |
| F4 | `keys_count()` is one request per L1 frame on every call, with no memo, although the archive is assumed immutable: about 9 800 ranged reads for a compact archive of 10 M keys |
| F5 | `PageCache`: the LRU stamp is `SystemTime::now()` on every hit (a clock stepped back makes the page just used the eviction victim; the writers' cache uses a monotonic tick); eviction is a linear scan under the write lock on every insert into a full cache, and `evict_down_to` rescans the map per page removed, so a bulk shrink is O(n²); a tokio `RwLock` whose guard is never held across an await is the only reason the split readers' cache setters are `async` |
| F6 | Readers guess an L1 page by the DEFAULT size: a working archive written with larger pages costs two reads per page and never forms `get_range` runs; the variable reader's first read is always 16 KiB, 16× what a 1 KiB page needs |
| F7 | Every flush that writes anything rewrites all linked L0 continuations and frame 0's entries, also when L0 did not change and only the 8-byte hash needs restoring |
| F8 | The records writer remaps all 1 440 cells on every insert, also for an append at the end of a frame — the hot path of rows written in time order ([writer_insert.rs:296](src/fixed_key_minute_indexed_records/writer_insert.rs#L296)) |
| F9 | `GlobalDayIndexReader::open` reads the 8-byte sync time and then L0 from offset 8 as two `read_at`s: over S3 a HEAD plus 3 GETs instead of 2. `read_from` is one `read_at` per login |
| F10 | The records index costs 12 968 bytes per key per day whatever the key holds: for a median account of 4 deals the L2 frame is about ten times its data (accepted in the spec) |

## Refuted

| Finding | Why it does not stand |
|---|---|
| `new_with_capacity` takes counts on the fixed and records writers and BYTES on the variable ones (`api-9`) | The facts hold, but it is a documented, deliberate decision: the parameters are `l0_frame_bytes` / `l1_page_bytes` and both doc comments open with "Both sizes in BYTES". What is left is a preference for another constructor name |
| `set_day` accepts 65 536 years per login but `to_bytes` cannot write more than 65 535 blocks (`day-index-5`) | It reproduces, but it is the documented behaviour: `to_bytes` lists "a login with more than 65 535 years" as a `ContractViolation`, a limit deferred to it like the 4 GiB one. It takes one login with a day in every year from 0 to 65 535 |
| `SplitIndexWriter::max_bytes` and its check exist only for tests (`split-write-5`) | The facts are right, and intended: a documented `#[cfg(test)]` seam through which the tests reach the index-full refusals |

## Checked and found right

The reviewers read these for errors and found none:

- The frame-0 hash protocol: every byte that reaches a file goes through a range check, then the spoil, then
  the IO; the state flag and the file can disagree only after a dropped future, which the in-flight guard has
  already poisoned. The early return of a flush with nothing written is safe.
- The append frontier and its accounting; the 4 GiB index limit counted with the L0 continuations the next
  flush needs; the 256 data files checked before anything is created; a page rolling over only whole.
- Placement Rule 1 and Rule 2 in both preallocated writers; in the fixed one, no await between the first L1
  change and the last, and no L1 page cached above its capacity.
- Frame decoding, chain walks, the open-time checks, the L1 and data runs of `get_range`, and that any error is
  an `Err`, never a partial `Ok`.
- The compaction plans against what the writers write in both preallocated modules, and the plan, the runs and
  the index order of the records compaction; determinism given `max_data_file_bytes`; a compact archive
  compacting to itself.
- The length prefix, the half-page arithmetic and the construction minimums of the variable module (8 179 /
  8 178 bytes, 16 386 / 16 384 bytes); the L0 continuation bound as an upper bound.
- The records place and overflow rules against decisions 3, 4, 5 and 7, with 400 random seeds of crowded
  minutes read back against a model, 150 more over up to 300 keys in random order, and 300 over random index
  limits; the read-ahead window; the compact form.
- The leap-year day numbering, the bit layout, the calendar for any `i64` including negative times, `to_bytes`
  determinism, Pascal strings.
- Every backticked identifier and file name of the old READMEs, and their header sizes, defaults, half-page
  numbers and compact size formulas, against the code.
- Nothing test-only is reachable in a release build.
