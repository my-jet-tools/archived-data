use crate::{ArchiveError, DATA_REF_LEN, DataRef, decode_key_prefix, le_u32, le_u64};

/// Walks the entries of one frame body or page body, which end at the end marker or at the end of the
/// bytes, whichever comes first. Shared by the SingleFile and the split formats: only the fields after a
/// key differ.
pub(crate) struct EntryWalk<'a> {
    bytes: &'a [u8],
    at: usize,
    place: &'a str,
}

impl<'a> EntryWalk<'a> {
    pub fn new(bytes: &'a [u8], at: usize, place: &'a str) -> Self {
        Self { bytes, at, place }
    }

    /// The key field starting the next entry: `None` where the entries end.
    pub fn first_key(&mut self) -> Result<Option<(usize, usize)>, ArchiveError> {
        if self.at >= self.bytes.len() {
            return Ok(None);
        }

        match decode_key_prefix(self.bytes, self.at).map_err(|err| self.located(err))? {
            None => Ok(None),
            Some((len, prefix)) => self.take_key(len, prefix).map(Some),
        }
    }

    /// A key field inside an entry, where neither the end nor the end marker may be.
    pub fn inner_key(&mut self) -> Result<(usize, usize), ArchiveError> {
        if self.at >= self.bytes.len() {
            return Err(self.cut_short());
        }

        match decode_key_prefix(self.bytes, self.at).map_err(|err| self.located(err))? {
            None => Err(ArchiveError::Corrupted(format!(
                "{} has an empty key inside an entry",
                self.place
            ))),
            Some((len, prefix)) => self.take_key(len, prefix),
        }
    }

    pub fn offset(&mut self) -> Result<u64, ArchiveError> {
        let at = self.field(8)?;
        Ok(le_u64(self.bytes, at))
    }

    pub fn u32_field(&mut self) -> Result<u32, ArchiveError> {
        let at = self.field(4)?;
        Ok(le_u32(self.bytes, at))
    }

    pub fn data_ref(&mut self) -> Result<DataRef, ArchiveError> {
        let at = self.field(DATA_REF_LEN)?;
        Ok(DataRef::decode(self.bytes, at))
    }

    /// Where a field of `len` bytes starts, stepping past it.
    fn field(&mut self, len: usize) -> Result<usize, ArchiveError> {
        if self.at + len > self.bytes.len() {
            return Err(self.cut_short());
        }

        let at = self.at;
        self.at += len;
        Ok(at)
    }

    fn take_key(&mut self, len: usize, prefix: usize) -> Result<(usize, usize), ArchiveError> {
        let key_at = self.at + prefix;

        if key_at + len > self.bytes.len() {
            return Err(self.cut_short());
        }

        self.at = key_at + len;
        Ok((key_at, len))
    }

    fn cut_short(&self) -> ArchiveError {
        ArchiveError::Corrupted(format!("{} has an entry cut short", self.place))
    }

    fn located(&self, err: ArchiveError) -> ArchiveError {
        match err {
            ArchiveError::Corrupted(message) => {
                ArchiveError::Corrupted(format!("{}: {message}", self.place))
            }
            other => other,
        }
    }
}

pub(crate) fn count_mismatch(place: &str, declared: usize, read: usize) -> ArchiveError {
    ArchiveError::Corrupted(format!(
        "{place} declares {declared} entries and holds {read}"
    ))
}
