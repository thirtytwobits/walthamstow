//! Engine operation journals and staging.
//!
//! A journal records how far an operation got, so a retry or a restart can
//! finish it. Journals belong to the engine, not to the workspace: they live
//! under the engine data directory beside the database. Durable workspace
//! content goes in a configured workspace path.
//!
//! The kernel lays journals and staging out; the part of the engine that keeps
//! one names it.

use std::path::{Path, PathBuf};

/// Whether `name` can name a journal or staging directory: one or more lowercase ASCII
/// letters, digits, `-` or `_`, and no device name Windows reserves.
const fn is_directory_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || crate::files::is_windows_device_name(name) {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        if !matches!(bytes[index], b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_') {
            return false;
        }
        index += 1;
    }
    true
}

/// A journal, named by the part of the engine that keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Journal(&'static str);

impl Journal {
    /// The journal named `name`, which must be one directory: lowercase ASCII letters, digits,
    /// `-` and `_`. Any other name panics, which fails the build where the journal is a constant.
    pub const fn new(name: &'static str) -> Self {
        assert!(
            is_directory_name(name),
            "a journal is named by lowercase letters, digits, '-' and '_'"
        );
        Self(name)
    }

    /// The journal's directory under the engine data directory `data_dir`.
    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join("journals").join(self.0)
    }

    /// The journal's directory, refused when it or the `journals` directory holding it is a
    /// link, so what is read from it and written to it stays under `data_dir`.
    pub fn unlinked_root(self, data_dir: &Path) -> std::io::Result<PathBuf> {
        unlinked(&data_dir.join("journals"), self.0)
    }
}

/// Where an operation stages what it promotes into place by rename, and parks
/// what it replaces, named by the part of the engine that runs it. Staging
/// belongs to the engine as a journal does, so it lives under the engine data
/// directory. What it holds is gone once the operation ends, or at the next
/// start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Staging(&'static str);

impl Staging {
    /// The staging named `name`, which must be one directory: lowercase ASCII letters, digits,
    /// `-` and `_`. Any other name panics, which fails the build where the staging is a constant.
    pub const fn new(name: &'static str) -> Self {
        assert!(
            is_directory_name(name),
            "staging is named by lowercase letters, digits, '-' and '_'"
        );
        Self(name)
    }

    /// The staging directory under the engine data directory `data_dir`.
    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join("staging").join(self.0)
    }

    /// The staging directory, refused when it or the `staging` directory holding it is a link,
    /// so what is staged, promoted or cleaned there stays under `data_dir`.
    pub fn unlinked_root(self, data_dir: &Path) -> std::io::Result<PathBuf> {
        unlinked(&data_dir.join("staging"), self.0)
    }
}

/// `parent` joined with `name`, refused when either is a link.
fn unlinked(parent: &Path, name: &str) -> std::io::Result<PathBuf> {
    let root = parent.join(name);
    for path in [parent, root.as_path()] {
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(std::io::Error::other(format!(
                "{} is a link, which the engine does not follow.",
                path.display()
            )));
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journals_and_staging_live_apart_under_the_data_directory_by_name() {
        let data_dir = Path::new("engine-data");
        let name = "an-operation";
        let journal = Journal::new(name).root(data_dir);
        let staging = Staging::new(name).root(data_dir);
        for root in [&journal, &staging] {
            assert!(root.starts_with(data_dir));
            assert!(root.ends_with(name));
        }
        assert_ne!(journal, staging);
        assert_ne!(journal, Journal::new("another-operation").root(data_dir));
    }

    #[test]
    fn a_name_that_is_not_one_plain_directory_is_refused() {
        for name in [
            "",
            "../outside",
            "/absolute",
            "a/b",
            "Upper",
            ".hidden",
            "con",
        ] {
            assert!(
                std::panic::catch_unwind(|| Journal::new(name)).is_err(),
                "{name:?}"
            );
            assert!(
                std::panic::catch_unwind(|| Staging::new(name)).is_err(),
                "{name:?}"
            );
        }
        Journal::new("collaboration-publications");
        Staging::new("session_imports");
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_reached_through_a_link_is_refused() {
        let journal = Journal::new("an-operation");
        let outside = tempfile::tempdir().unwrap();
        for linked in ["journals", "journals/an-operation"] {
            let data = tempfile::tempdir().unwrap();
            let link = data.path().join(linked);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            assert!(journal.unlinked_root(data.path()).is_err(), "{linked}");
        }
        let data = tempfile::tempdir().unwrap();
        assert_eq!(
            journal.unlinked_root(data.path()).unwrap(),
            journal.root(data.path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_reached_through_a_link_is_refused() {
        let staging = Staging::new("an-operation");
        let outside = tempfile::tempdir().unwrap();
        for linked in ["staging", "staging/an-operation"] {
            let data = tempfile::tempdir().unwrap();
            let link = data.path().join(linked);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            assert!(staging.unlinked_root(data.path()).is_err(), "{linked}");
        }
        let data = tempfile::tempdir().unwrap();
        assert_eq!(
            staging.unlinked_root(data.path()).unwrap(),
            staging.root(data.path())
        );
    }
}
