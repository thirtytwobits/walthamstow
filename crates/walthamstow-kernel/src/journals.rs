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

/// A journal, named by the part of the engine that keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Journal(&'static str);

impl Journal {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The journal's directory under the engine data directory `data_dir`.
    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join("journals").join(self.0)
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
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The staging directory under the engine data directory `data_dir`.
    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join("staging").join(self.0)
    }
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
}
