use std::fmt;

#[derive(Debug)]
pub enum ArchiveError {
    /// The sink or the source failed.
    Io(std::io::Error),
    /// The bytes are not a file of this format, or not one of these key/slot/item sizes.
    Corrupted(String),
    /// A trait implementation broke its contract: a wrong byte count, a slot out of range.
    ContractViolation(String),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "archive io: {err}"),
            Self::Corrupted(message) => write!(f, "archive corrupted: {message}"),
            Self::ContractViolation(message) => write!(f, "archive contract violation: {message}"),
        }
    }
}

impl std::error::Error for ArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ArchiveError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// So a producer returning `io::Result` — the upload writer's — can use `?` on the archive.
impl From<ArchiveError> for std::io::Error {
    fn from(err: ArchiveError) -> Self {
        match err {
            ArchiveError::Io(err) => err,
            other => std::io::Error::new(std::io::ErrorKind::InvalidData, other),
        }
    }
}
