//! The limits of the split archive. Each one is a field width, not a tuning choice.

use crate::ArchiveError;

/// Every offset stored in the index is a `u32`, so the index holds at most `u32::MAX + 1` bytes.
pub const MAX_INDEX_BYTES: u64 = 1 << 32;

/// A data reference names its data file in one byte.
pub const MAX_DATA_FILES: usize = 256;

/// A data page is addressed by a `u32` offset inside its data file.
pub const MAX_DATA_FILE_BYTES: u64 = 1 << 32;

pub const DEFAULT_MAX_DATA_FILE_BYTES: u64 = MAX_DATA_FILE_BYTES;

/// Where `len` more index bytes start when the index is `end` bytes long, and the index's length after
/// them. Growing past [`MAX_INDEX_BYTES`] is refused, so every offset handed out fits `u32`.
pub(crate) fn reserve_index_bytes(end: u64, len: usize) -> Result<(u32, u64), ArchiveError> {
    let grown = end
        .checked_add(len as u64)
        .filter(|grown| *grown <= MAX_INDEX_BYTES);

    match (u32::try_from(end), grown) {
        (Ok(offset), Some(grown)) => Ok((offset, grown)),
        _ => Err(ArchiveError::ContractViolation(format!(
            "{len} more bytes at {end} would grow the index past {MAX_INDEX_BYTES} bytes"
        ))),
    }
}

/// A construction-time contract, like the capacity checks: a page is never split across data files, so a
/// data file must hold at least one, and `u32` offsets reach no further than 4 GiB.
pub(crate) fn check_max_data_file_bytes(bytes: u64, page_len: usize) {
    assert!(page_len > 0, "a data page must not be empty");
    assert!(
        bytes <= MAX_DATA_FILE_BYTES,
        "a data file may not exceed 4 GiB: offsets into it are u32"
    );
    assert!(
        bytes >= page_len as u64,
        "a data file must hold at least one data page"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_grows_to_exactly_four_gib_and_no_further() {
        assert_eq!(reserve_index_bytes(0, 12).unwrap(), (0, 12));
        assert_eq!(
            reserve_index_bytes((1 << 32) - 12, 12).unwrap(),
            (u32::MAX - 11, 1 << 32)
        );

        for (end, len) in [((1 << 32) - 11, 12), (1 << 32, 0), (u64::MAX, 1)] {
            assert!(matches!(
                reserve_index_bytes(end, len),
                Err(ArchiveError::ContractViolation(_))
            ));
        }
    }

    #[test]
    fn a_data_file_of_one_page_or_of_four_gib_is_accepted() {
        check_max_data_file_bytes(100, 100);
        check_max_data_file_bytes(DEFAULT_MAX_DATA_FILE_BYTES, 100);
    }

    #[test]
    #[should_panic(expected = "at least one data page")]
    fn a_data_file_smaller_than_a_page_panics() {
        check_max_data_file_bytes(99, 100);
    }

    #[test]
    #[should_panic(expected = "4 GiB")]
    fn a_data_file_past_four_gib_panics() {
        check_max_data_file_bytes((1 << 32) + 1, 100);
    }
}
