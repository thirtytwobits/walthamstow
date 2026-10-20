//! Copyright (c) Scott A Dixon
//!
//! Publishing staged files over live ones, each fenced by the hash it held when staged.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use walthamstow_kernel::durability::{write_synced, WriteDurability};
use walthamstow_kernel::hashing::sha256_hex;

/// One file a publication replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    /// Relative to the live and staged roots alike.
    pub path: PathBuf,
    /// The live file's hash when it was staged, or `None` when there was none.
    pub source_hash: Option<String>,
    pub target_hash: String,
}

/// The hash of the file at `path`, or `None` when there is none.
pub fn file_hash(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(sha256_hex(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Publishes each replacement from `staged` into `live`. Every replacement is checked before
/// any is written: a live file already at its target is left alone, one still at its source is
/// replaced, and one that is neither refuses the whole publication. Publishing again after an
/// interruption finishes it.
pub fn publish(
    durability: WriteDurability,
    live: &Path,
    staged: &Path,
    replacements: &[Replacement],
) -> Result<()> {
    let mut publications = Vec::new();
    for replacement in replacements {
        if replacement.path.as_os_str().is_empty()
            || replacement.path.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("Invalid publication path {}.", replacement.path.display());
        }
        let current = file_hash(&live.join(&replacement.path))?;
        if current.as_ref() == Some(&replacement.target_hash) {
            continue;
        }
        if current != replacement.source_hash {
            bail!(
                "{} changed after it was staged.",
                replacement.path.display()
            );
        }
        let bytes = std::fs::read(staged.join(&replacement.path))?;
        if sha256_hex(&bytes) != replacement.target_hash {
            bail!(
                "The staged {} does not match its journal.",
                replacement.path.display()
            );
        }
        publications.push((&replacement.path, bytes));
    }
    for (path, bytes) in publications {
        write_synced(durability, &live.join(path), &bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const DURABILITY: WriteDurability = WriteDurability::System;

    /// A file to stage: its path, its live bytes when it has any, and its staged bytes.
    type StagedFile<'a> = (&'a str, Option<&'a [u8]>, &'a [u8]);

    /// Live and staged roots, with `files` staged.
    fn stage(files: &[StagedFile<'_>]) -> (tempfile::TempDir, tempfile::TempDir, Vec<Replacement>) {
        let live = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let mut replacements = Vec::new();
        for (path, current, target) in files {
            if let Some(current) = current {
                write_synced(DURABILITY, &live.path().join(path), current).unwrap();
            }
            write_synced(DURABILITY, &staged.path().join(path), target).unwrap();
            replacements.push(Replacement {
                path: PathBuf::from(path),
                source_hash: current.map(sha256_hex),
                target_hash: sha256_hex(target),
            });
        }
        (live, staged, replacements)
    }

    fn contents(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        walthamstow_kernel::files::read_files(root).unwrap()
    }

    #[test]
    fn a_publication_replaces_each_file_and_creates_the_new_ones() {
        let (live, staged, replacements) = stage(&[
            ("kept/a.json", Some(b"old a"), b"new a"),
            ("b.json", None, b"new b"),
        ]);
        publish(DURABILITY, live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(contents(live.path()), contents(staged.path()));
    }

    #[test]
    fn publishing_again_after_an_interruption_finishes_the_publication() {
        let (live, staged, replacements) = stage(&[
            ("a.json", Some(b"old a"), b"new a"),
            ("b.json", Some(b"old b"), b"new b"),
        ]);
        std::fs::copy(staged.path().join("a.json"), live.path().join("a.json")).unwrap();
        publish(DURABILITY, live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(contents(live.path()), contents(staged.path()));
        publish(DURABILITY, live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(contents(live.path()), contents(staged.path()));
    }

    #[test]
    fn a_file_changed_after_staging_refuses_the_whole_publication() {
        let (live, staged, replacements) = stage(&[
            ("a.json", Some(b"old a"), b"new a"),
            ("b.json", Some(b"old b"), b"new b"),
        ]);
        write_synced(DURABILITY, &live.path().join("b.json"), b"edited b").unwrap();
        let before = contents(live.path());
        assert!(publish(DURABILITY, live.path(), staged.path(), &replacements).is_err());
        assert_eq!(contents(live.path()), before);
    }

    #[test]
    fn staging_that_does_not_match_its_journal_is_not_published() {
        let (live, staged, replacements) = stage(&[("a.json", Some(b"old a"), b"new a")]);
        write_synced(DURABILITY, &staged.path().join("a.json"), b"tampered").unwrap();
        let before = contents(live.path());
        assert!(publish(DURABILITY, live.path(), staged.path(), &replacements).is_err());
        assert_eq!(contents(live.path()), before);
    }

    #[test]
    fn a_path_outside_the_roots_is_refused() {
        let live = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        for path in [
            "../escape.json",
            "kept/../../escape.json",
            "/absolute.json",
            "",
        ] {
            let replacement = Replacement {
                path: PathBuf::from(path),
                source_hash: None,
                target_hash: sha256_hex(b"bytes"),
            };
            assert!(
                publish(DURABILITY, live.path(), staged.path(), &[replacement]).is_err(),
                "{path}"
            );
        }
    }
}
