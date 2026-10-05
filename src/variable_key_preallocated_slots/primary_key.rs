use crate::{ArchiveError, MAX_VARIABLE_KEY_LEN};

/// The key a data page belongs to, of any length from 1 to [`MAX_VARIABLE_KEY_LEN`] bytes. The file
/// sorts on `Ord`, so the byte form does not have to sort the same way.
pub trait PrimaryKeyVariableSize: Ord + Clone {
    /// `src` is exactly the bytes [`Self::copy_to`] wrote, without the length prefix.
    fn from_slice(src: &[u8]) -> Self;

    /// Appends ONLY the key bytes; the crate writes the length prefix. The crate checks the length.
    fn copy_to(&self, out: &mut Vec<u8>);
}

/// The length check every key write goes through, out of reach of the implementations.
pub(crate) trait CheckedVariableKeyBytes {
    fn copy_variable_key_checked_to(&self, out: &mut Vec<u8>) -> Result<(), ArchiveError>;
}

impl<TKey: PrimaryKeyVariableSize> CheckedVariableKeyBytes for TKey {
    fn copy_variable_key_checked_to(&self, out: &mut Vec<u8>) -> Result<(), ArchiveError> {
        let before = out.len();
        self.copy_to(out);
        let written = out.len() - before;

        if written == 0 {
            return Err(ArchiveError::ContractViolation(
                "PrimaryKeyVariableSize::copy_to wrote an empty key".to_string(),
            ));
        }

        if written > MAX_VARIABLE_KEY_LEN {
            return Err(ArchiveError::ContractViolation(format!(
                "PrimaryKeyVariableSize::copy_to wrote {written} bytes, the largest key is \
                 {MAX_VARIABLE_KEY_LEN}"
            )));
        }

        Ok(())
    }
}
