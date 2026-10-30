//! Copyright (c) Scott A Dixon
//!
//! Publishing staged files over live ones, each fenced by the hash it held when staged. The live
//! and staged roots are `cap-std` directories, and every replacement resolves through them: a
//! path or a link that would leave either root fails.

use crate::durability::{write_synced_in, WriteDurability};
use crate::hashing::sha256_hex;
use crate::reason_codes::{PUBLICATION_PATHS_COLLIDE, PUBLICATION_STALE};
use crate::KernelError;
use anyhow::{bail, Result};
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// One file a publication replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    /// Relative to the live and staged roots alike.
    pub path: PathBuf,
    /// The live file's hash when it was staged, or `None` when there was none.
    pub source_hash: Option<String>,
    /// The staged file's hash, or `None` when the publication removes the live file.
    pub target_hash: Option<String>,
}

/// The hash of the file at `path` within `dir`, or `None` when there is none. Anything there but
/// a regular file is refused unopened.
pub fn file_hash(dir: &Dir, path: &Path) -> Result<Option<String>> {
    match dir.symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            bail!(
                "{} is not a regular file; it is not opened.",
                path.display()
            )
        }
        Ok(_) => Ok(Some(sha256_hex(&dir.read(path)?))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The bytes of the regular file at `path` within `dir`. Anything else there is refused
/// unopened.
pub fn read_regular(dir: &Dir, path: &Path) -> Result<Vec<u8>> {
    if !dir.symlink_metadata(path)?.is_file() {
        bail!(
            "{} is not a regular file; it is not opened.",
            path.display()
        );
    }
    Ok(dir.read(path)?)
}

/// Refuses `path` unless it is relative and names a file beneath the root it is joined to
/// by its normal components alone, so no two spellings name one file.
pub fn check_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("Invalid publication path {}.", path.display());
    }
    Ok(())
}

/// Refuses `paths` unless [`check_relative_path`] accepts each and no two differ only in case.
/// Two that do are refused as a conflict with the reason [`PUBLICATION_PATHS_COLLIDE`], naming
/// the second as `path`, since a file system that folds case holds them as one file.
pub fn check_publication_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Result<()> {
    let mut named = std::collections::BTreeSet::new();
    for path in paths {
        check_relative_path(path)?;
        if !named.insert(path.to_string_lossy().to_lowercase()) {
            return Err(KernelError::conflict(format!(
                "{} is named twice, compared case-folded, in one publication.",
                path.display()
            ))
            .with_data(serde_json::json!({ "path": path }))
            .with_reason(PUBLICATION_PATHS_COLLIDE)
            .into());
        }
    }
    Ok(())
}

/// Held from a publication's first check to its last write, so two publications from one
/// source cannot both pass their fences. One engine owns a workspace, so a lock within the
/// process is enough.
static PUBLISHING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Publishes each replacement from `staged` into `live`. Every replacement is checked before
/// any is written: a live file already at its target is left alone, one still at its source is
/// replaced or removed, and one that is neither refuses the whole publication. A file that is
/// neither is refused as a conflict with the reason [`PUBLICATION_STALE`], naming its `path`.
/// Publishing again after an interruption finishes it.
pub fn publish(
    durability: WriteDurability,
    live: &Dir,
    staged: &Dir,
    replacements: &[Replacement],
) -> Result<()> {
    publish_as(durability, live, staged, replacements, true)
}

/// Publishes each replacement from `staged` into `live` for the first time, as [`publish`]
/// does, except that every live file must still be at its source: one already at its target
/// refuses the whole publication too, since nothing has published it yet.
pub fn publish_first(
    durability: WriteDurability,
    live: &Dir,
    staged: &Dir,
    replacements: &[Replacement],
) -> Result<()> {
    publish_as(durability, live, staged, replacements, false)
}

/// What a publication does to one live file.
enum Change<'a> {
    Write(&'a Path, Vec<u8>),
    Remove(&'a Path),
}

/// Publishes each replacement, accepting a live file already at its target when `resuming`.
fn publish_as(
    durability: WriteDurability,
    live: &Dir,
    staged: &Dir,
    replacements: &[Replacement],
    resuming: bool,
) -> Result<()> {
    let _publishing = PUBLISHING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_publication_paths(
        replacements
            .iter()
            .map(|replacement| replacement.path.as_path()),
    )?;
    let mut changes = Vec::new();
    for replacement in replacements {
        let path = replacement.path.as_path();
        refuse_linked_directories(live, path)?;
        let current = file_hash(live, path)?;
        if resuming && current == replacement.target_hash {
            // An interrupted publication may have written or removed it without syncing its
            // entry.
            sync_holder(durability, live, path)?;
            continue;
        }
        if current != replacement.source_hash {
            return Err(KernelError::conflict(format!(
                "{} changed after it was staged.",
                path.display()
            ))
            .with_data(serde_json::json!({ "path": path }))
            .with_reason(PUBLICATION_STALE)
            .into());
        }
        let Some(target_hash) = &replacement.target_hash else {
            changes.push(Change::Remove(path));
            continue;
        };
        let bytes = read_regular(staged, path)?;
        if sha256_hex(&bytes) != *target_hash {
            bail!("The staged {} does not match its journal.", path.display());
        }
        changes.push(Change::Write(path, bytes));
    }
    for change in changes {
        match change {
            Change::Write(path, bytes) => write_synced_in(durability, live, path, &bytes)?,
            Change::Remove(path) => {
                match live.remove_file(path) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(error.into())
                    }
                    _ => {}
                }
                sync_holder(durability, live, path)?;
            }
        }
    }
    Ok(())
}

/// Refuses `path` within `dir` when a directory on its way is a link. Two paths through a linked
/// directory can name one file, which one publication would then write twice.
fn refuse_linked_directories(dir: &Dir, path: &Path) -> Result<()> {
    let mut walked = PathBuf::new();
    for part in path.parent().into_iter().flat_map(Path::components) {
        walked.push(part);
        match dir.symlink_metadata(&walked) {
            Ok(metadata) if metadata.is_symlink() => {
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

/// Makes the entries of the directory holding `path` within `dir` durable. A directory that does
/// not exist has no entries to make durable.
fn sync_holder(durability: WriteDurability, dir: &Dir, path: &Path) -> Result<()> {
    match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        None => durability.sync_dir(dir)?,
        Some(parent) => match dir.open_dir(parent) {
            Ok(holder) => durability.sync_dir(&holder)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durability::write_synced;
    use std::collections::BTreeMap;

    const DURABILITY: WriteDurability = WriteDurability::System;

    /// A file to stage: its path, its live bytes when it has any, and its staged bytes, or none
    /// when the publication removes it.
    type StagedFile<'a> = (&'a str, Option<&'a [u8]>, Option<&'a [u8]>);

    /// The directory at `path`, held as a `Dir`.
    fn dir(path: &Path) -> Dir {
        Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
    }

    /// Live and staged roots, with `files` staged.
    fn stage(files: &[StagedFile<'_>]) -> (tempfile::TempDir, tempfile::TempDir, Vec<Replacement>) {
        let live = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let mut replacements = Vec::new();
        for (path, current, target) in files {
            if let Some(current) = current {
                write_synced(DURABILITY, &live.path().join(path), current).unwrap();
            }
            if let Some(target) = target {
                write_synced(DURABILITY, &staged.path().join(path), target).unwrap();
            }
            replacements.push(Replacement {
                path: PathBuf::from(path),
                source_hash: current.map(sha256_hex),
                target_hash: target.map(sha256_hex),
            });
        }
        (live, staged, replacements)
    }

    fn publish_at(live: &Path, staged: &Path, replacements: &[Replacement]) -> Result<()> {
        publish(DURABILITY, &dir(live), &dir(staged), replacements)
    }

    fn contents(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        crate::files::read_files(root).unwrap()
    }

    #[test]
    fn a_publication_replaces_each_file_and_creates_the_new_ones() {
        let (live, staged, replacements) = stage(&[
            ("kept/a.json", Some(b"old a"), Some(b"new a")),
            ("b.json", None, Some(b"new b")),
        ]);
        publish_at(live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(contents(live.path()), contents(staged.path()));
    }

    #[test]
    fn a_publication_removes_a_file_it_has_no_target_for() {
        let (live, staged, replacements) = stage(&[
            ("kept/a.json", Some(b"old a"), None),
            ("b.json", Some(b"old b"), Some(b"new b")),
            ("c.json", None, None),
        ]);
        publish_at(live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(contents(live.path()), contents(staged.path()));
    }

    #[test]
    fn publishing_again_after_an_interruption_finishes_the_publication() {
        let (live, staged, replacements) = stage(&[
            ("a.json", Some(b"old a"), Some(b"new a")),
            ("b.json", Some(b"old b"), Some(b"new b")),
            ("c.json", Some(b"old c"), None),
        ]);
        std::fs::copy(staged.path().join("a.json"), live.path().join("a.json")).unwrap();
        std::fs::remove_file(live.path().join("c.json")).unwrap();
        publish_at(live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(contents(live.path()), contents(staged.path()));
        publish_at(live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(contents(live.path()), contents(staged.path()));
    }

    #[test]
    fn files_already_published_are_settled_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let (live, staged, replacements) =
                stage(&[("kept/a.json", Some(b"old a"), Some(b"new a"))]);
            write_synced(durability, &live.path().join("kept/a.json"), b"new a").unwrap();
            publish(
                durability,
                &dir(live.path()),
                &dir(staged.path()),
                &replacements,
            )
            .unwrap();
            assert_eq!(contents(live.path()), contents(staged.path()));
        }
    }

    #[test]
    fn a_file_changed_after_staging_refuses_the_whole_publication() {
        let (live, staged, replacements) = stage(&[
            ("a.json", Some(b"old a"), Some(b"new a")),
            ("b.json", Some(b"old b"), None),
        ]);
        write_synced(DURABILITY, &live.path().join("b.json"), b"edited b").unwrap();
        let before = contents(live.path());
        let refused = publish_at(live.path(), staged.path(), &replacements).unwrap_err();
        let refused = refused.downcast_ref::<KernelError>().unwrap();
        assert_eq!(refused.reason_code(), Some(PUBLICATION_STALE));
        assert_eq!(refused.data.as_ref().unwrap()["path"], "b.json");
        assert_eq!(contents(live.path()), before);
    }

    #[test]
    fn a_first_publication_refuses_a_file_already_at_its_target() {
        let (live, staged, replacements) = stage(&[
            ("a.json", Some(b"old a"), Some(b"new a")),
            ("b.json", Some(b"old b"), Some(b"new b")),
        ]);
        write_synced(DURABILITY, &live.path().join("b.json"), b"new b").unwrap();
        let before = contents(live.path());
        let refused = publish_first(
            DURABILITY,
            &dir(live.path()),
            &dir(staged.path()),
            &replacements,
        )
        .unwrap_err();
        let refused = refused.downcast_ref::<KernelError>().unwrap();
        assert_eq!(refused.reason_code(), Some(PUBLICATION_STALE));
        assert_eq!(refused.data.as_ref().unwrap()["path"], "b.json");
        assert_eq!(contents(live.path()), before);
        publish_at(live.path(), staged.path(), &replacements).unwrap();
        assert_eq!(std::fs::read(live.path().join("a.json")).unwrap(), b"new a");
    }

    #[test]
    fn staging_that_does_not_match_its_journal_is_not_published() {
        let (live, staged, replacements) = stage(&[("a.json", Some(b"old a"), Some(b"new a"))]);
        write_synced(DURABILITY, &staged.path().join("a.json"), b"tampered").unwrap();
        let before = contents(live.path());
        let refused = publish_at(live.path(), staged.path(), &replacements).unwrap_err();
        assert!(refused
            .downcast_ref::<KernelError>()
            .and_then(KernelError::reason_code)
            .is_none());
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
                target_hash: Some(sha256_hex(b"bytes")),
            };
            assert!(
                publish_at(live.path(), staged.path(), &[replacement]).is_err(),
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_publication_does_not_write_through_a_link_leaving_the_root() {
        let outside = tempfile::tempdir().unwrap();
        for link in ["linked", "linked/a.json"] {
            let (live, staged, replacements) = stage(&[("linked/a.json", None, Some(b"new a"))]);
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
                publish_at(live.path(), staged.path(), &replacements).is_err(),
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
    fn a_publication_does_not_write_through_a_link_within_the_root() {
        let (live, staged, replacements) = stage(&[
            ("real/a.json", Some(b"old a"), Some(b"first")),
            ("alias/a.json", None, Some(b"second")),
        ]);
        std::os::unix::fs::symlink("real", live.path().join("alias")).unwrap();
        let replacements = replacements
            .into_iter()
            .map(|replacement| Replacement {
                source_hash: Some(sha256_hex(b"old a")),
                ..replacement
            })
            .collect::<Vec<_>>();
        let error = publish_at(live.path(), staged.path(), &replacements).unwrap_err();
        assert!(error.to_string().contains("is a link"), "{error}");
        assert_eq!(
            std::fs::read(live.path().join("real/a.json")).unwrap(),
            b"old a"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_special_live_file_refuses_the_publication_unopened() {
        let (live, staged, replacements) = stage(&[("a.json", None, Some(b"new a"))]);
        let _special = std::os::unix::net::UnixListener::bind(live.path().join("a.json")).unwrap();
        let error = publish_at(live.path(), staged.path(), &replacements).unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_staged_file_is_not_published() {
        let outside = tempfile::tempdir().unwrap();
        let (live, staged, replacements) = stage(&[("a.json", Some(b"old a"), Some(b"new a"))]);
        std::fs::write(outside.path().join("a.json"), b"new a").unwrap();
        std::fs::remove_file(staged.path().join("a.json")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("a.json"), staged.path().join("a.json"))
            .unwrap();
        assert!(publish_at(live.path(), staged.path(), &replacements).is_err());
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
                target_hash: Some(sha256_hex(target)),
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
                    publish_at(live, staged, std::slice::from_ref(replacement)).is_ok()
                })
            });
            runs.map(|run| run.join().unwrap())
        });
        assert_eq!(outcomes.iter().filter(|published| **published).count(), 1);
    }

    #[test]
    fn a_publication_naming_one_file_twice_is_refused_unwritten() {
        for (first, second) in [
            ("a.json", "./a.json"),
            ("a.json", "A.json"),
            ("a.json", "a.json"),
        ] {
            let (live, staged, mut replacements) = stage(&[(first, Some(b"old"), Some(b"new"))]);
            replacements.push(Replacement {
                path: PathBuf::from(second),
                source_hash: Some(sha256_hex(b"old")),
                target_hash: Some(sha256_hex(b"new")),
            });
            let before = contents(live.path());
            let refused = publish_at(live.path(), staged.path(), &replacements).unwrap_err();
            if second == "A.json" || second == "a.json" {
                assert_eq!(
                    refused
                        .downcast_ref::<KernelError>()
                        .and_then(KernelError::reason_code),
                    Some(PUBLICATION_PATHS_COLLIDE),
                    "{first} and {second}"
                );
            }
            assert_eq!(contents(live.path()), before);
        }
    }
}
