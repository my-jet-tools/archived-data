use crate::{ArchiveError, MAX_VARIABLE_KEY_LEN};

/// The LAST key that starts with `prefix`: `prefix` followed by `0xFF` up to [`MAX_VARIABLE_KEY_LEN`] bytes
/// in all — `prefix` itself when it is that long already.
///
/// For a key whose `Ord` is the order of its bytes (`Vec<u8>`, `String`, a byte slice), `prefix ..= this`
/// is EXACTLY every key that starts with `prefix`, whatever its length: a key starting with `prefix` has at
/// most `MAX_VARIABLE_KEY_LEN − prefix.len()` bytes after it, and those sort at or below as many `0xFF`s;
/// a key at or above `prefix` that does not start with it differs from it first at a greater byte, and
/// sorts past every key that does. So
/// `reader.get_range(&prefix, &last_key_starting_with(&prefix)?, …)` answers "every key under `prefix`".
///
/// A shorter run of `0xFF` does not: `prefix ..= prefix + [0xFF; n]` holds only keys that start with
/// `prefix`, but not all of them — it drops those whose first `n` bytes after `prefix` are all `0xFF` and
/// that go on past them — and it says nothing about length, holding keys far longer than `n` bytes past
/// `prefix`.
///
/// The key is 32 767 bytes long, and is only ever compared: a comparison stops at the first byte that
/// differs. For a key whose `Ord` is not its bytes' order this bound means nothing.
///
/// A `prefix` longer than [`MAX_VARIABLE_KEY_LEN`] is a `ContractViolation`: no key starts with it.
pub fn last_key_starting_with(prefix: &[u8]) -> Result<Vec<u8>, ArchiveError> {
    if prefix.len() > MAX_VARIABLE_KEY_LEN {
        return Err(ArchiveError::ContractViolation(format!(
            "a prefix of {} bytes is longer than a key may be: {MAX_VARIABLE_KEY_LEN} bytes",
            prefix.len()
        )));
    }

    let mut last = Vec::with_capacity(MAX_VARIABLE_KEY_LEN);
    last.extend_from_slice(prefix);
    last.resize(MAX_VARIABLE_KEY_LEN, 0xFF);

    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_is_filled_with_ff_to_the_longest_key() {
        for prefix in [&b""[..], b"node/", &[0xFF; 3], &[0x00; 40]] {
            let last = last_key_starting_with(prefix).unwrap();

            assert_eq!(last.len(), MAX_VARIABLE_KEY_LEN);
            assert!(last.starts_with(prefix));
            assert!(last[prefix.len()..].iter().all(|byte| *byte == 0xFF));
        }

        let longest = vec![7u8; MAX_VARIABLE_KEY_LEN];
        assert_eq!(last_key_starting_with(&longest).unwrap(), longest);
    }

    #[test]
    fn a_prefix_longer_than_any_key_is_refused() {
        assert!(matches!(
            last_key_starting_with(&vec![1u8; MAX_VARIABLE_KEY_LEN + 1]),
            Err(ArchiveError::ContractViolation(_))
        ));
    }

    #[test]
    fn the_keys_between_the_prefix_and_its_last_key_are_exactly_the_keys_that_start_with_it() {
        let prefix = b"node/".to_vec();
        let last = last_key_starting_with(&prefix).unwrap();

        let longest_under = [
            &prefix[..],
            &vec![0xFF; MAX_VARIABLE_KEY_LEN - prefix.len()],
        ]
        .concat();
        let keys: Vec<(Vec<u8>, bool)> = vec![
            (b"node.".to_vec(), false),
            (b"node".to_vec(), false),
            (prefix.clone(), true),
            (b"node/a".to_vec(), true),
            (b"node/ab".to_vec(), true),
            (b"node/abcdefgh".to_vec(), true),
            ([&prefix[..], &[0xFF, 0x01]].concat(), true),
            ([&prefix[..], &[0xFF; 200], &[0x01]].concat(), true),
            (longest_under, true),
            (b"node0".to_vec(), false),
            (b"nodf".to_vec(), false),
        ];

        for (key, starts) in keys {
            assert_eq!(prefix <= key && key <= last, starts, "{key:?}");
            assert_eq!(key.starts_with(&prefix), starts, "{key:?}");
        }
    }
}
