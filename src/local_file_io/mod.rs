//! The files of ONE archive in a local directory: both factories of the split IO over the file system.
//!
//! [`LocalFileIo`] is a directory. It hands out the index as the file [`index_file_name`] and data file `n` as
//! [`data_file_name`]`(n)` — `index`, `data00` … `dataff` — which are also the names an uploader gives the
//! objects, so an archive compacted into a directory is uploaded file by file under the same names.
//!
//! [`LocalFileWriter`] and [`LocalFileReader`] do positional IO — `pwrite` / `pread` on unix — on a blocking
//! thread, so a reader holds no position and concurrent reads of one file do not wait on each other, and a
//! reader sees every byte a writer of the same file has written, the moment its call returned.

mod blocking_file_ops;
mod factory;
mod file_names;
mod reader;
#[cfg(test)]
mod test_dir;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_round_trip;
mod writer;

pub(crate) use blocking_file_ops::*;
pub use factory::*;
pub use file_names::*;
pub use reader::*;
pub use writer::*;
