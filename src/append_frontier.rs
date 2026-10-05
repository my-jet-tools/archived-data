use crate::ArchiveError;

/// The two ends of a file that grows only by `append`: `reserved` is how far offsets have been handed
/// out, `appended` how far bytes have reached the file. A writer holds everything between the two in its
/// caches and appends it in offset order, which is what lets an offset be handed out before its bytes
/// exist without ever leaving a hole.
pub(crate) struct AppendFrontier {
    appended: u64,
    reserved: u64,
}

impl AppendFrontier {
    pub fn new(appended: u64) -> Self {
        Self {
            appended,
            reserved: appended,
        }
    }

    /// The offset `len` more bytes will start at.
    pub fn reserve(&mut self, len: usize) -> u64 {
        let offset = self.reserved;
        self.reserved += len as u64;
        offset
    }

    pub fn appended(&self) -> u64 {
        self.appended
    }

    pub fn reserved(&self) -> u64 {
        self.reserved
    }

    pub fn is_appended(&self, offset: u64) -> bool {
        offset < self.appended
    }

    /// Before the append: it must start at the frontier and stay inside what was reserved.
    pub fn check_append(&self, offset: u64, len: usize) -> Result<(), ArchiveError> {
        if offset != self.appended || offset + len as u64 > self.reserved {
            return Err(ArchiveError::ContractViolation(format!(
                "{len} bytes appended at {offset}; the file is appended up to {} and reserved up to {}",
                self.appended, self.reserved
            )));
        }

        Ok(())
    }

    /// After the append: the file must have put the bytes at the frontier.
    pub fn confirm_append(&mut self, returned: u64, len: usize) -> Result<(), ArchiveError> {
        if returned != self.appended {
            return Err(ArchiveError::ContractViolation(format!(
                "an append of {len} bytes landed at {returned}, not at the end of the {}-byte file",
                self.appended
            )));
        }

        self.appended += len as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_handed_out_ahead_of_the_bytes() {
        let mut frontier = AppendFrontier::new(12);

        assert_eq!(frontier.reserve(10), 12);
        assert_eq!(frontier.reserve(5), 22);
        assert_eq!((frontier.appended(), frontier.reserved()), (12, 27));
        assert!(!frontier.is_appended(12));

        frontier.check_append(12, 10).unwrap();
        frontier.confirm_append(12, 10).unwrap();

        assert!(frontier.is_appended(21));
        assert!(!frontier.is_appended(22));
        assert_eq!(frontier.appended(), 22);
    }

    #[test]
    fn an_append_off_the_frontier_or_past_the_reservation_is_refused() {
        let mut frontier = AppendFrontier::new(0);
        frontier.reserve(10);
        frontier.reserve(10);

        for (offset, len) in [(10, 10), (0, 21)] {
            assert!(matches!(
                frontier.check_append(offset, len),
                Err(ArchiveError::ContractViolation(_))
            ));
        }

        assert!(matches!(
            frontier.confirm_append(4, 10),
            Err(ArchiveError::ContractViolation(_))
        ));
        assert_eq!(frontier.appended(), 0);
    }
}
