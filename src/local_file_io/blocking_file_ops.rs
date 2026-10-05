use std::fs::File;
use std::path::Path;

/// Runs one blocking file operation on tokio's blocking pool. A panic inside it is resumed here; an
/// operation the runtime never ran — it is shutting down — is an error, never a success.
pub(crate) async fn run_blocking_file_op<T: Send + 'static>(
    operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    match tokio::task::spawn_blocking(operation).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(std::io::Error::other(format!(
            "a blocking file operation did not run: {error}"
        ))),
    }
}

/// `error` with what was being done and to which file, its kind kept — a missing file stays `NotFound`.
pub(crate) fn file_io_error(error: std::io::Error, doing: &str, path: &Path) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("{doing} {}: {error}", path.display()))
}

/// All of `bytes` at `offset`, whatever the file's position — which is not used and not relied on: on
/// Windows `seek_write` / `seek_read` move it, on unix `pwrite` / `pread` do not.
pub(crate) fn pwrite_all(file: &File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(file, bytes, offset)
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;

        let (mut bytes, mut offset) = (bytes, offset);

        while !bytes.is_empty() {
            match file.seek_write(bytes, offset) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(written) => {
                    bytes = &bytes[written..];
                    offset += written as u64;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }

        Ok(())
    }
}

/// Exactly `buf.len()` bytes from `offset`; a file that ends first is `UnexpectedEof`.
pub(crate) fn pread_exact(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;

        let (mut buf, mut offset) = (buf, offset);

        while !buf.is_empty() {
            match file.seek_read(buf, offset) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(read) => {
                    buf = &mut buf[read..];
                    offset += read as u64;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }

        Ok(())
    }
}
