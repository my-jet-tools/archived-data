//! A string inside a record: a Pascal string, `len u8` then `len` bytes of UTF-8.
//!
//! A string longer than 255 bytes is CUT to at most 255 on a character boundary: a character that does not
//! fit whole is dropped, never split, so what is read back is always valid UTF-8 and a prefix of what was
//! written. The archive never looks inside a record; these are for the code that encodes one.

use crate::ArchiveError;

/// The most bytes of a string a Pascal string keeps: its length is one byte.
pub const PASCAL_STRING_MAX_BYTES: usize = u8::MAX as usize;

/// Appends `value` as a Pascal string, cut to at most [`PASCAL_STRING_MAX_BYTES`] on a character boundary,
/// and returns how many bytes of `value` it kept.
pub fn write_pascal_string(value: &str, out: &mut Vec<u8>) -> usize {
    let mut len = value.len().min(PASCAL_STRING_MAX_BYTES);

    while !value.is_char_boundary(len) {
        len -= 1;
    }

    out.push(len as u8);
    out.extend_from_slice(&value.as_bytes()[..len]);
    len
}

/// The Pascal string at the start of `src`, and the bytes after it. No length byte, fewer bytes than the
/// length states, or bytes that are not UTF-8 is `Corrupted`.
pub fn read_pascal_string(src: &[u8]) -> Result<(&str, &[u8]), ArchiveError> {
    let Some((len, rest)) = src.split_first() else {
        return Err(ArchiveError::Corrupted(
            "a Pascal string has no length byte".to_string(),
        ));
    };

    let len = *len as usize;

    if rest.len() < len {
        return Err(ArchiveError::Corrupted(format!(
            "a Pascal string states {len} bytes and {} follow",
            rest.len()
        )));
    }

    let (bytes, rest) = rest.split_at(len);

    let value = std::str::from_utf8(bytes).map_err(|error| {
        ArchiveError::Corrupted(format!(
            "a Pascal string of {len} bytes is not UTF-8: {error}"
        ))
    })?;

    Ok((value, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(value: &str) -> (Vec<u8>, usize) {
        let mut out = vec![0xAA];
        let kept = write_pascal_string(value, &mut out);
        (out, kept)
    }

    #[test]
    fn a_string_round_trips_with_the_bytes_after_it() {
        let mut out = Vec::new();
        write_pascal_string("EURUSD", &mut out);
        write_pascal_string("", &mut out);
        write_pascal_string("Ключ", &mut out);
        out.push(7);

        assert_eq!(&out[..7], b"\x06EURUSD");
        assert_eq!(out[7], 0);

        let (first, rest) = read_pascal_string(&out).unwrap();
        let (second, rest) = read_pascal_string(rest).unwrap();
        let (third, rest) = read_pascal_string(rest).unwrap();

        assert_eq!(
            (first, second, third, rest),
            ("EURUSD", "", "Ключ", &[7u8][..])
        );
    }

    #[test]
    fn a_string_of_255_bytes_is_kept_whole_and_a_longer_one_is_cut_to_255() {
        let exact = "a".repeat(255);
        let (out, kept) = written(&exact);
        assert_eq!((out[1], kept, out.len()), (255, 255, 257));

        let long = "b".repeat(300);
        let (out, kept) = written(&long);
        assert_eq!((out[1], kept), (255, 255));
        assert_eq!(
            read_pascal_string(&out[1..]).unwrap(),
            (&long[..255], &[][..])
        );
    }

    #[test]
    fn a_character_that_does_not_fit_whole_is_dropped() {
        // 254 ASCII bytes, then a 2-byte character: byte 255 would split it.
        let two = format!("{}ж", "x".repeat(254));
        let (out, kept) = written(&two);
        assert_eq!(kept, 254);
        assert_eq!(read_pascal_string(&out[1..]).unwrap().0, "x".repeat(254));

        // 253 ASCII bytes, then a 3-byte character: it ends exactly at 256, one past the limit.
        let three = format!("{}€€", "x".repeat(253));
        let (out, kept) = written(&three);
        assert_eq!(kept, 253);
        assert_eq!(out[1], 253);

        // A 4-byte character ending exactly at 255 is kept.
        let four = format!("{}😀tail", "x".repeat(251));
        let (out, kept) = written(&four);
        assert_eq!(kept, 255);
        assert_eq!(
            read_pascal_string(&out[1..]).unwrap().0,
            format!("{}😀", "x".repeat(251))
        );

        // Nothing but 3-byte characters: 85 of them are 255 bytes.
        let (_, kept) = written(&"€".repeat(100));
        assert_eq!(kept, 255);
        let (_, kept) = written(&format!("a{}", "€".repeat(100)));
        assert_eq!(kept, 253);
    }

    #[test]
    fn a_missing_length_a_short_body_or_bytes_that_are_not_utf8_are_corrupted() {
        for bytes in [
            vec![],
            vec![3, b'a', b'b'],
            vec![2, 0xC3, 0x28],
            vec![1, 0xFF],
        ] {
            assert!(
                matches!(read_pascal_string(&bytes), Err(ArchiveError::Corrupted(_))),
                "{bytes:?}"
            );
        }
    }
}
