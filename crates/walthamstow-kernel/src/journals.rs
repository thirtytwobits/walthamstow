//! Engine operation journals and staging.
//!
//! A journal records how far an operation got, so a retry or a restart can
//! finish it. Journals belong to the engine, not to the workspace: they live
//! under the engine data directory beside the database. Durable workspace
//! content goes in a configured workspace path.
//!
//! The kernel lays journals and staging out; the part of the engine that keeps
//! one names it. Each is reached through the data directory held as a
//! `cap-std` directory, and a link at `journals/`, `staging/` or the directory
//! itself is refused unfollowed.

use crate::durability::{Access, WriteDurability};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, DirBuilder};
use std::io;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// The name the journal was given.
    pub const fn name(self) -> &'static str {
        self.0
    }

    /// Where the journal's directory lies under the engine data directory `data_dir`.
    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join(JOURNALS).join(self.0)
    }

    /// The journal's directory in the data directory `data`, or `None` when it does not exist.
    pub fn dir(self, data: &Dir) -> io::Result<Option<Dir>> {
        open_unlinked(data, JOURNALS, self.0)
    }

    /// The journal's directory in the data directory `data`, created with every missing level
    /// when it does not exist, each new entry durable as `durability` asks.
    pub fn create_dir(self, durability: WriteDurability, data: &Dir) -> io::Result<Dir> {
        self.create_dir_as(durability, data, Access::Inherited)
    }

    /// [`Journal::create_dir`], with each directory it creates reachable as `access` says.
    pub(crate) fn create_dir_as(
        self,
        durability: WriteDurability,
        data: &Dir,
        access: Access,
    ) -> io::Result<Dir> {
        create_unlinked(durability, data, JOURNALS, self.0, access)
    }
}

/// The directory holding every journal.
const JOURNALS: &str = "journals";

/// The directory holding every staging directory.
const STAGING: &str = "staging";

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

    /// Where the staging directory lies under the engine data directory `data_dir`.
    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join(STAGING).join(self.0)
    }

    /// The staging directory in the data directory `data`, or `None` when it does not exist.
    pub fn dir(self, data: &Dir) -> io::Result<Option<Dir>> {
        open_unlinked(data, STAGING, self.0)
    }

    /// The staging directory in the data directory `data`, created with every missing level
    /// when it does not exist, each new entry durable as `durability` asks.
    pub fn create_dir(self, durability: WriteDurability, data: &Dir) -> io::Result<Dir> {
        create_unlinked(durability, data, STAGING, self.0, Access::Inherited)
    }
}

/// The directory `parent/name` in `data`, or `None` when either level does not exist. A link at
/// either level is refused unfollowed.
fn open_unlinked(data: &Dir, parent: &str, name: &str) -> io::Result<Option<Dir>> {
    let Some(parent) = open_level(data, parent)? else {
        return Ok(None);
    };
    open_level(&parent, name)
}

/// The directory `name` in `dir`, or `None` when it does not exist. A link there is refused
/// unfollowed.
fn open_level(dir: &Dir, name: &str) -> io::Result<Option<Dir>> {
    match dir.open_dir_nofollow(name) {
        Ok(opened) => Ok(Some(opened)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The directory `parent/name` in `data`, each level created when it does not exist and
/// reachable as `access` says. A link at either level is refused unfollowed.
fn create_unlinked(
    durability: WriteDurability,
    data: &Dir,
    parent: &str,
    name: &str,
    access: Access,
) -> io::Result<Dir> {
    let parent = create_level(durability, data, parent, access)?;
    create_level(durability, &parent, name, access)
}

/// The directory `name` in `dir`, created when it does not exist with its entry made durable.
/// A creation whose sync fails is undone, so a directory found existing has a durable entry.
fn create_level(
    durability: WriteDurability,
    dir: &Dir,
    name: &str,
    access: Access,
) -> io::Result<Dir> {
    if let Some(opened) = open_level(dir, name)? {
        return Ok(opened);
    }
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    if access == Access::Owner {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    #[cfg(not(unix))]
    let _ = access;
    let created = match dir.create_dir_with(name, &builder) {
        Ok(()) => true,
        // Another caller made it meanwhile, and may not have synced its entry yet.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error),
    };
    if let Err(error) = durability.sync_dir(dir) {
        if created {
            let _ = dir.remove_dir(name);
        }
        return Err(error);
    }
    dir.open_dir_nofollow(name)
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

    /// The data directory at `path`, held as a `Dir`.
    fn data(path: &Path) -> Dir {
        crate::files::open_dir(path).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_or_staging_reached_through_a_link_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        for (parent, name) in [("journals", "an-operation"), ("staging", "an-operation")] {
            let open = |data: &Dir| {
                if parent == "journals" {
                    Journal::new(name).dir(data).map(|dir| dir.is_some())
                } else {
                    Staging::new(name).dir(data).map(|dir| dir.is_some())
                }
            };
            let create = |data: &Dir| {
                if parent == "journals" {
                    Journal::new(name).create_dir(WriteDurability::System, data)
                } else {
                    Staging::new(name).create_dir(WriteDurability::System, data)
                }
            };
            for linked in [parent.to_owned(), format!("{parent}/{name}")] {
                for within in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let target = if within {
                        let target = root.path().join("elsewhere");
                        std::fs::create_dir(&target).unwrap();
                        target
                    } else {
                        outside.path().to_path_buf()
                    };
                    let link = root.path().join(&linked);
                    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
                    std::os::unix::fs::symlink(&target, &link).unwrap();
                    let data = data(root.path());
                    assert!(open(&data).is_err(), "{linked} within {within}");
                    assert!(create(&data).is_err(), "{linked} within {within}");
                }
            }
            let root = tempfile::tempdir().unwrap();
            let data = data(root.path());
            assert!(!open(&data).unwrap());
            create(&data).unwrap();
            assert!(open(&data).unwrap());
            assert!(root.path().join(parent).join(name).is_dir());
        }
    }
}
