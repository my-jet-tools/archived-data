//! A directory of its own for every test, under the system's temporary directory, removed with everything in
//! it when the test ends — so the tests need no crate for it and never see each other's files.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// A new, empty directory; `name` only makes it recognisable.
    pub fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "archived-data-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::AcqRel)
        ));

        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("clearing {}: {error}", path.display()),
        }

        std::fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("creating {}: {error}", path.display()));

        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The names of the files in `dir`, sorted.
    pub fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("listing {}: {error}", dir.display()))
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let removed = std::fs::remove_dir_all(&self.path);

        if let Err(error) = removed
            && !std::thread::panicking()
        {
            panic!("removing {}: {error}", self.path.display());
        }
    }
}
