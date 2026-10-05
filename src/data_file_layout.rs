use crate::{ArchiveError, DataRef, MAX_DATA_FILES, check_max_data_file_bytes};

/// Where the data page of the key of `rank` lands. Every data page has the same length and data files
/// are filled in order, each with as many whole pages as `max_data_file_bytes` holds, so the place is a
/// closed form of the rank and nothing about the files has to be remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DataFileLayout {
    page_len: usize,
    max_data_file_bytes: u64,
    pages_per_file: u64,
}

impl DataFileLayout {
    /// Panics when a data file of `max_data_file_bytes` cannot hold one page, or exceeds 4 GiB.
    pub fn new(page_len: usize, max_data_file_bytes: u64) -> Self {
        check_max_data_file_bytes(max_data_file_bytes, page_len);

        Self {
            page_len,
            max_data_file_bytes,
            pages_per_file: max_data_file_bytes / page_len as u64,
        }
    }

    pub fn page_len(&self) -> usize {
        self.page_len
    }

    pub fn max_data_file_bytes(&self) -> u64 {
        self.max_data_file_bytes
    }

    pub fn pages_per_file(&self) -> u64 {
        self.pages_per_file
    }

    /// A rank that would need data file 256 is a `ContractViolation`.
    pub fn ref_of(&self, rank: usize) -> Result<DataRef, ArchiveError> {
        let rank = rank as u64;
        let file = rank / self.pages_per_file;

        let Ok(file_no) = u8::try_from(file) else {
            return Err(ArchiveError::ContractViolation(format!(
                "data page {rank} at {} pages per file needs data file {file}; there are {MAX_DATA_FILES}",
                self.pages_per_file
            )));
        };

        // Below `max_data_file_bytes - page_len`, which is below 4 GiB.
        let offset = (rank % self.pages_per_file) * self.page_len as u64;

        Ok(DataRef {
            file_no,
            offset: offset as u32,
        })
    }

    /// The size of every data file `pages` pages fill, in file order; none for no pages.
    pub fn file_sizes(&self, pages: usize) -> Result<Vec<u64>, ArchiveError> {
        let pages = pages as u64;
        let files = pages.div_ceil(self.pages_per_file);

        if files > MAX_DATA_FILES as u64 {
            return Err(ArchiveError::ContractViolation(format!(
                "{pages} data pages at {} per file need {files} data files; there are {MAX_DATA_FILES}",
                self.pages_per_file
            )));
        }

        Ok((0..files)
            .map(|file| {
                let in_file = (pages - file * self.pages_per_file).min(self.pages_per_file);
                in_file * self.page_len as u64
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_MAX_DATA_FILE_BYTES;

    fn at(file_no: u8, offset: u32) -> DataRef {
        DataRef { file_no, offset }
    }

    #[test]
    fn a_page_goes_into_a_file_only_if_it_fits_whole() {
        let exact = DataFileLayout::new(100, 200);
        assert_eq!(exact.pages_per_file(), 2);
        assert_eq!(exact.page_len(), 100);
        assert_eq!(exact.max_data_file_bytes(), 200);

        let refs: Vec<DataRef> = (0..5).map(|rank| exact.ref_of(rank).unwrap()).collect();
        assert_eq!(refs, [at(0, 0), at(0, 100), at(1, 0), at(1, 100), at(2, 0)]);

        let short = DataFileLayout::new(100, 199);
        assert_eq!(short.pages_per_file(), 1);
        assert_eq!(short.ref_of(1).unwrap(), at(1, 0));
    }

    #[test]
    fn data_file_256_is_a_contract_violation() {
        let layout = DataFileLayout::new(100, 200);

        assert_eq!(layout.ref_of(256 * 2 - 1).unwrap(), at(255, 100));
        assert!(matches!(
            layout.ref_of(256 * 2),
            Err(ArchiveError::ContractViolation(_))
        ));

        assert_eq!(layout.file_sizes(256 * 2).unwrap().len(), 256);
        assert!(matches!(
            layout.file_sizes(256 * 2 + 1),
            Err(ArchiveError::ContractViolation(_))
        ));
    }

    #[test]
    fn file_sizes_are_whole_files_then_the_remainder() {
        let layout = DataFileLayout::new(100, 200);

        assert_eq!(layout.file_sizes(0).unwrap(), Vec::<u64>::new());
        assert_eq!(layout.file_sizes(4).unwrap(), [200, 200]);
        assert_eq!(layout.file_sizes(5).unwrap(), [200, 200, 100]);
    }

    #[test]
    fn every_offset_of_a_four_gib_file_fits_u32() {
        let layout = DataFileLayout::new(3082, DEFAULT_MAX_DATA_FILE_BYTES);
        let per_file = layout.pages_per_file() as usize;

        let last = layout.ref_of(per_file - 1).unwrap();
        assert_eq!(last.file_no, 0);
        assert!(last.offset as u64 + 3082 <= DEFAULT_MAX_DATA_FILE_BYTES);
        assert_eq!(layout.ref_of(per_file).unwrap(), at(1, 0));
    }
}
