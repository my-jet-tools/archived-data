use crate::ArchiveError;

/// The key a data page belongs to. Its byte form is what the file sorts on only through `Ord`, so the
/// byte order of the encoding does not have to agree with it.
pub trait PrimaryKeyFixedSize: Ord + Clone {
    const LEN: usize;

    /// `src` is exactly [`Self::LEN`] bytes.
    fn from_slice(src: &[u8]) -> Self;

    /// Appends exactly [`Self::LEN`] bytes. The crate checks it.
    fn copy_to(&self, out: &mut Vec<u8>);
}

/// The length check every key write goes through, out of reach of the implementations.
pub(crate) trait CheckedKeyBytes {
    fn copy_key_checked_to(&self, out: &mut Vec<u8>) -> Result<(), ArchiveError>;
}

impl<TKey: PrimaryKeyFixedSize> CheckedKeyBytes for TKey {
    fn copy_key_checked_to(&self, out: &mut Vec<u8>) -> Result<(), ArchiveError> {
        let before = out.len();
        self.copy_to(out);
        let written = out.len() - before;

        if written != TKey::LEN {
            return Err(ArchiveError::ContractViolation(format!(
                "PrimaryKeyFixedSize::copy_to wrote {written} bytes, LEN is {}",
                TKey::LEN
            )));
        }

        Ok(())
    }
}
