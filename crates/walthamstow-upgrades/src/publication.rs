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

/// The hash of the file at `path`, or `None` when there is none. Anything there but a regular
/// file is refused unopened.
pub fn file_hash(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!(
                "{} is not a regular file; it is not opened.",
                path.display()
            )
        }
        Ok(_) => Ok(Some(sha256_hex(&std::fs::read(path)?))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Refuses `path` unless it is relative and stays beneath the root it is joined to.
pub fn check_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("Invalid publication path {}.", path.display());
    }
    Ok(())
}

/// The bytes of the regular file at `path`. Anything else there is refused unopened.
pub fn read_regular(path: &Path) -> Result<Vec<u8>> {
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        bail!(
            "{} is not a regular file; it is not opened.",
            path.display()
        );
    }
    Ok(std::fs::read(path)?)
}

/// Held from a publication's first check to its last write, so two publications from one
/// source cannot both pass their fences. One engine owns a workspace, so a lock within the
/// process is enough.
static PUBLISHING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Refuses `path` beneath `root` when any part of it that exists is a link, so a write to it
/// cannot leave `root`.
fn refuse_links(root: &Path, path: &Path) -> Result<()> {
    let mut walked = root.to_path_buf();
    for part in path.components() {
        walked.push(part);
        match std::fs::symlink_metadata(&walked) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "{} is a link, which a publication does not write through.",
                    walked.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Publishes each replacement from `staged` into `live`. Every replacement is checked before
/// any is written: a live file already at its target is left alone, one still at its source is
/// replaced, and one that is neither, or that lies through a link, refuses the whole
/// publication. Publishing again after an interruption finishes it.
pub fn publish(
    durability: WriteDurability,
    live: &Path,
    staged: &Path,
    replacements: &[Replacement],
) -> Result<()> {
    let _publishing = PUBLISHING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut publications = Vec::new();
    for replacement in replacements {
        check_relative_path(&replacement.path)?;
        refuse_links(live, &replacement.path)?;
        let live_path = live.join(&replacement.path);
        let current = file_hash(&live_path)?;
        if current.as_ref() == Some(&replacement.target_hash) {
            // An interrupted publication may have written it without syncing its entry.
            durability
                .sync_directory(live_path.parent().expect("a published file has a parent"))?;
            continue;
        }
        if current != replacement.source_hash {
            bail!(
                "{} changed after it was staged.",
                replacement.path.display()
            );
        }
        refuse_links(staged, &replacement.path)?;
        let bytes = read_regular(&staged.join(&replacement.path))?;
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
    fn files_already_published_are_settled_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let (live, staged, replacements) = stage(&[("kept/a.json", Some(b"old a"), b"new a")]);
            write_synced(durability, &live.path().join("kept/a.json"), b"new a").unwrap();
            publish(durability, live.path(), staged.path(), &replacements).unwrap();
            assert_eq!(contents(live.path()), contents(staged.path()));
        }
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

    #[cfg(unix)]
    #[test]
    fn a_publication_does_not_write_through_a_link() {
        let outside = tempfile::tempdir().unwrap();
        for link in ["linked", "linked/a.json"] {
            let (live, staged, replacements) = stage(&[("linked/a.json", None, b"new a")]);
            write_synced(DURABILITY, &outside.path().join("a.json"), b"old a").unwrap();
            let replacements = replacements
                .into_iter()
                .map(|replacement| Replacement {
                    source_hash: Some(sha256_hex(b"old a")),
                    ..replacement
                })
                .collect::<Vec<_>>();
            let target = if link == "linked" {
                outside.path().to_path_buf()
            } else {
                std::fs::create_dir_all(live.path().join("linked")).unwrap();
                outside.path().join("a.json")
            };
            std::os::unix::fs::symlink(&target, live.path().join(link)).unwrap();
            assert!(
                publish(DURABILITY, live.path(), staged.path(), &replacements).is_err(),
                "{link}"
            );
            assert_eq!(
                std::fs::read(outside.path().join("a.json")).unwrap(),
                b"old a"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_special_live_file_refuses_the_publication_unopened() {
        let (live, staged, replacements) = stage(&[("a.json", None, b"new a")]);
        let _special = std::os::unix::net::UnixListener::bind(live.path().join("a.json")).unwrap();
        let error = publish(DURABILITY, live.path(), staged.path(), &replacements).unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_staged_file_is_not_published() {
        let outside = tempfile::tempdir().unwrap();
        let (live, staged, replacements) = stage(&[("a.json", Some(b"old a"), b"new a")]);
        std::fs::write(outside.path().join("a.json"), b"new a").unwrap();
        std::fs::remove_file(staged.path().join("a.json")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("a.json"), staged.path().join("a.json"))
            .unwrap();
        assert!(publish(DURABILITY, live.path(), staged.path(), &replacements).is_err());
        assert_eq!(std::fs::read(live.path().join("a.json")).unwrap(), b"old a");
    }

    #[test]
    fn two_publications_from_one_source_cannot_both_succeed() {
        let live = tempfile::tempdir().unwrap();
        write_synced(DURABILITY, &live.path().join("a.json"), b"old").unwrap();
        let stage_target = |target: &[u8]| {
            let staged = tempfile::tempdir().unwrap();
            write_synced(DURABILITY, &staged.path().join("a.json"), target).unwrap();
            let replacement = Replacement {
                path: PathBuf::from("a.json"),
                source_hash: Some(sha256_hex(b"old")),
                target_hash: sha256_hex(target),
            };
            (staged, replacement)
        };
        let (first, first_replacement) = stage_target(b"first");
        let (second, second_replacement) = stage_target(b"second");
        let start = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let runs = [
                (first.path(), &first_replacement),
                (second.path(), &second_replacement),
            ]
            .map(|(staged, replacement)| {
                let start = &start;
                let live = live.path();
                scope.spawn(move || {
                    start.wait();
                    publish(DURABILITY, live, staged, std::slice::from_ref(replacement)).is_ok()
                })
            });
            runs.map(|run| run.join().unwrap())
        });
        assert_eq!(outcomes.iter().filter(|published| **published).count(), 1);
    }
}
