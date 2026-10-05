# pascal-strings

Strings inside a record. The archive reads nothing of a record but its time, the first 8 bytes of a [minute-indexed record](fixed-key-minute-indexed-records.md). Everything after them belongs to the record's own encoder, which writes each string with `write_pascal_string` and reads it back with `read_pascal_string`. Use them for every string field, so that every encoder cuts and checks strings the same way. Source: [pascal_string.rs](../src/pascal_string.rs).

| Item | Signature | What it does |
| --- | --- | --- |
| `PASCAL_STRING_MAX_BYTES` | `pub const PASCAL_STRING_MAX_BYTES: usize = u8::MAX as usize` | 255: the most bytes of a string that a Pascal string keeps, because its length is one byte. |
| `write_pascal_string` | `pub fn write_pascal_string(value: &str, out: &mut Vec<u8>) -> usize` | Appends the length byte and then the bytes. Returns how many bytes of `value` it kept. |
| `read_pascal_string` | `pub fn read_pascal_string(src: &[u8]) -> Result<(&str, &[u8]), ArchiveError>` | Returns the string at the start of `src` and the bytes after it. |

```text
len u8 · len bytes of UTF-8        len = 0 ..= 255; "" is the single byte 0x00
```

## A record with strings

A deal is the time, then a symbol, then a comment. The archive orders the deal by its time and keeps the rest as bytes. The strings come back exactly as they were written.

```rust
use archived_data::*;
use rust_extensions::date_time::DateTimeAsMicroseconds;

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

/// time i64 LE · symbol · comment
#[derive(Debug, PartialEq)]
struct Deal {
    time: i64,
    symbol: String,
    comment: String,
}

impl Deal {
    fn encode(&self) -> Vec<u8> {
        let mut out = self.time.to_le_bytes().to_vec();
        write_pascal_string(&self.symbol, &mut out);
        write_pascal_string(&self.comment, &mut out);
        out
    }

    fn decode(record: &[u8]) -> Result<Self, ArchiveError> {
        let Some((time, rest)) = record.split_first_chunk::<8>() else {
            return Err(ArchiveError::Corrupted("a deal shorter than its time".to_string()));
        };

        let (symbol, rest) = read_pascal_string(rest)?;
        let (comment, rest) = read_pascal_string(rest)?;

        if !rest.is_empty() {
            return Err(ArchiveError::Corrupted(format!("{} bytes after a deal", rest.len())));
        }

        Ok(Self {
            time: i64::from_le_bytes(*time),
            symbol: symbol.to_string(),
            comment: comment.to_string(),
        })
    }
}

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join(format!("archived-data-docs-pascal-strings-1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let day_start = DateTimeAsMicroseconds::new(1_767_225_600_000_000); // 2026-01-01 00:00 UTC
    let deal = Deal {
        time: day_start.unix_microseconds + 90_000_000,
        symbol: "EURUSD".to_string(),
        comment: "closed by TP".to_string(),
    };

    let record = deal.encode();
    assert_eq!(record.len(), 8 + (1 + 6) + (1 + 12));
    assert_eq!(&record[8..15], b"\x06EURUSD");

    let mut writer =
        FixedKeyMinuteIndexedRecordsArchiveWriter::new(LocalFileIo::new(&dir), "deals", day_start)
            .await
            .unwrap();
    writer.write(Login(7), &record).await.unwrap();
    writer.finish().await.unwrap();

    let reader = FixedKeyMinuteIndexedRecordsArchiveReader::<Login, _, _>::open(LocalFileIo::new(&dir), day_start)
        .await
        .unwrap();
    let records = reader.get_day(&Login(7)).await.unwrap();
    assert_eq!(records, [record]);
    assert_eq!(Deal::decode(&records[0]).unwrap(), deal);

    std::fs::remove_dir_all(&dir).unwrap();
}
```

`write_pascal_string(value, &mut out) -> usize`:

- Appends to `out` and never clears it, so a record's fields are written one after another into one buffer.
- Writes 1 length byte, then the first `len` bytes of `value`. `len` is `value.len()` when that is 255 or less. An empty string is one byte, `0x00`.
- Returns `len`, the number of bytes of `value` kept. When it is below `value.len()`, the string was cut (see the next section).
- Never fails and never panics.

`read_pascal_string(src) -> Result<(&str, &[u8]), ArchiveError>`:

- Reads the length byte and that many bytes, and nothing past them.
- Returns the string and the rest of `src`. Both borrow from `src`: nothing is copied. To read the next field, pass the rest back in.
- Fails with `Corrupted` when the bytes are not a Pascal string (see the last section), and never panics.

A string field takes at most 256 bytes of its record. A minute-indexed record holds at most `MAX_RECORD_LEN` = 8 192 bytes including its time (see [fixed-key-minute-indexed-records](fixed-key-minute-indexed-records.md)).

## A string longer than 255 bytes

A string longer than 255 bytes is cut to at most 255 bytes, on a character boundary. A character that does not fit whole is dropped, never split.

```rust
use archived_data::*;

fn main() {
    // 254 ASCII bytes, then "ж" (2 bytes): the cut at 255 would split it, so it is dropped.
    let value = format!("{}ж", "x".repeat(254));
    assert_eq!(value.len(), 256);

    let mut out = Vec::new();
    let kept = write_pascal_string(&value, &mut out);
    assert_eq!(kept, 254);
    assert_eq!((out[0], out.len()), (254, 1 + 254));

    let (read, rest) = read_pascal_string(&out).unwrap();
    assert_eq!(read, "x".repeat(254));
    assert!(value.starts_with(read));
    assert!(rest.is_empty());

    // 85 characters of 3 bytes are exactly 255 bytes, so the 86th is the first one dropped.
    let euros = "€".repeat(100);
    let mut out = Vec::new();
    assert_eq!(write_pascal_string(&euros, &mut out), PASCAL_STRING_MAX_BYTES);
    assert_eq!(read_pascal_string(&out).unwrap().0, "€".repeat(85));
}
```

- The kept part is the longest prefix of `value` that has at most 255 bytes and ends on a character boundary. For a string longer than 255 bytes, `kept` is 252 to 255, because a UTF-8 character is at most 4 bytes.
- What is read back is always valid UTF-8, and it is a prefix of what was written.
- The cut is silent. An encoder that must not lose text compares the returned `kept` with `value.len()`.

## Bytes that are not a Pascal string

```rust
use archived_data::*;

fn main() {
    let cases: [(&[u8], &str); 3] = [
        (&[], "a Pascal string has no length byte"),
        (&[3, b'a', b'b'], "a Pascal string states 3 bytes and 2 follow"),
        (&[1, 0xFF], "a Pascal string of 1 bytes is not UTF-8: invalid utf-8 sequence of 1 bytes from index 0"),
    ];

    for (bytes, message) in cases {
        let error = read_pascal_string(bytes).unwrap_err();
        assert!(matches!(error, ArchiveError::Corrupted(_)));
        assert_eq!(error.to_string(), format!("archive corrupted: {message}"));
    }
}
```

| `src` | Result |
| --- | --- |
| empty, so no length byte | `Corrupted("a Pascal string has no length byte")` |
| fewer bytes after the length byte than it states | `Corrupted("a Pascal string states {len} bytes and {n} follow")` |
| `len` bytes that are not UTF-8 | `Corrupted("a Pascal string of {len} bytes is not UTF-8: {utf8 error}")` |

A decoder that reads its strings with `?`, like `Deal::decode` above, returns these as its own `ArchiveError`. The other kinds of `ArchiveError` are in [errors](errors.md).
