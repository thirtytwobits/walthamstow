//! Copyright (c) Scott A Dixon
//!
//! The directories a workspace keeps its documents in, each configured relative to its root and
//! held open through the root's handle.

use anyhow::{bail, Context, Result};
use cap_std::fs::Dir;
use std::path::{Component, Path, PathBuf};
use walthamstow_kernel::durability::{create_dir_all_in, WriteDurability};
use walthamstow_kernel::files::{identity, Identity};

/// A configured directory, held open. A link within the root changed later does not move it.
#[derive(Debug)]
pub struct ConfiguredDirectory {
    pub dir: Dir,
    /// Where the directory is configured, relative to the root, with `/` between components.
    pub relative: String,
}

/// How many levels a configured directory may lie beneath the root, links on the way to it
/// included.
const MAX_DEPTH: usize = 256;

/// Resolves each `(name, configured)` directory through the workspace root `root`, creating each
/// that does not exist as durably as `durability` asks. A directory must be a normalised
/// relative path in ASCII, every component a name Windows allows, and no two may overlap,
/// compared case-folded or through a link within the root, since a case-insensitive file system
/// holds two that differ only in case as one. A directory that would leave the root, or where a
/// part of it that exists is not a directory, fails through the root's handle. A refusal names
/// each directory by its `name`.
pub fn resolve_directories<const N: usize>(
    durability: WriteDurability,
    root: &Dir,
    configured: [(&str, &str); N],
) -> Result<[ConfiguredDirectory; N]> {
    for (name, value) in configured {
        check_name(name, value)?;
    }
    let folded = configured.map(|(_, value)| PathBuf::from(value.to_lowercase()));
    refuse_overlap(&configured, |left, right| {
        folded[left].starts_with(&folded[right]) || folded[right].starts_with(&folded[left])
    })?;
    let root_identity = identity(root).context("Failed to identify the workspace root.")?;
    let mut held = Vec::with_capacity(N);
    let mut ancestries = Vec::with_capacity(N);
    for (name, value) in configured {
        let dir = open_or_create(durability, root, name, value)?;
        ancestries.push(ancestry(root, &root_identity, name, value, &dir)?);
        held.push(ConfiguredDirectory {
            dir,
            relative: value.to_owned(),
        });
    }
    refuse_overlap(&configured, |left, right| {
        ancestries[left].contains(&ancestries[right][0])
            || ancestries[right].contains(&ancestries[left][0])
    })?;
    Ok(held
        .try_into()
        .expect("one resolved directory for each configured one"))
}

/// Refuses the first two configured directories `overlap` says overlap, naming both.
fn refuse_overlap(
    configured: &[(&str, &str)],
    overlap: impl Fn(usize, usize) -> bool,
) -> Result<()> {
    for left in 0..configured.len() {
        for right in (left + 1)..configured.len() {
            if overlap(left, right) {
                bail!(
                    "{} and {} must not overlap.",
                    configured[left].0,
                    configured[right].0
                );
            }
        }
    }
    Ok(())
}

/// Refuses `configured` unless it names a directory by the rules every configured directory
/// meets: a non-empty, normalised relative path in ASCII, each component a name Windows allows.
fn check_name(name: &str, configured: &str) -> Result<()> {
    if configured.is_empty() || configured.contains('\\') {
        bail!("{name} must be a non-empty relative path using forward slashes.");
    }
    if !configured.is_ascii() {
        bail!("{name} must be spelled in ASCII.");
    }
    let relative = Path::new(configured);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("{name} must stay within the workspace root.");
    }
    if relative.components().any(|component| {
        !walthamstow_kernel::files::windows_allows(&component.as_os_str().to_string_lossy())
    }) {
        bail!("{name} has a directory name Windows does not allow.");
    }
    let normalised = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if normalised != configured {
        bail!("{name} must be a normalised relative path.");
    }
    Ok(())
}

/// The directory `configured` names through `root`, created with every missing level when it
/// does not exist. A link within the root on the way is followed; one that would leave it fails.
fn open_or_create(
    durability: WriteDurability,
    root: &Dir,
    name: &str,
    configured: &str,
) -> Result<Dir> {
    let refused = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::anyhow!("{name} must stay within the workspace root: {error}")
        } else {
            anyhow::Error::from(error).context(format!(
                "{name} must be a directory within the workspace root."
            ))
        }
    };
    create_dir_all_in(durability, root, Path::new(configured)).map_err(refused)?;
    root.open_dir(configured).map_err(refused)
}

/// The identities of `dir`, which `configured` names through `root`, and of each directory
/// holding it up to the root, nearest first. A directory reached through a link is held by where
/// the link leads, so its parents are those of the link's target.
fn ancestry(
    root: &Dir,
    root_identity: &Identity,
    name: &str,
    configured: &str,
    dir: &Dir,
) -> Result<Vec<Identity>> {
    let mut chain = vec![identity(dir).with_context(|| format!("Failed to identify {name}."))?];
    if &chain[0] == root_identity {
        bail!("{name} must lie beneath the workspace root, not be it.");
    }
    let mut up = PathBuf::from(configured);
    for _ in 0..MAX_DEPTH {
        up.push("..");
        let parent = root
            .open_dir(&up)
            .with_context(|| format!("Failed to resolve the directories holding {name}."))?;
        let parent = identity(&parent).with_context(|| format!("Failed to identify {name}."))?;
        if &parent == root_identity {
            return Ok(chain);
        }
        chain.push(parent);
    }
    bail!("{name} lies more than {MAX_DEPTH} directories beneath the workspace root.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves `configured` through the workspace root at `root`.
    fn resolve<const N: usize>(
        root: &Path,
        configured: [(&str, &str); N],
    ) -> Result<[ConfiguredDirectory; N]> {
        let root = walthamstow_kernel::files::open_dir(root).unwrap();
        resolve_directories(WriteDurability::System, &root, configured)
    }

    #[test]
    fn each_directory_is_held_beneath_the_root_and_made_when_absent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/present")).unwrap();
        let configured = [("present", "data/present"), ("absent", "data/absent")];
        let resolved = resolve(root.path(), configured).unwrap();
        for ((_, relative), directory) in configured.iter().zip(&resolved) {
            assert_eq!(directory.relative, *relative);
            directory.dir.write("held.yaml", b"held").unwrap();
            assert!(root.path().join(relative).join("held.yaml").is_file());
        }
    }

    #[test]
    fn a_directory_outside_the_root_or_not_normalised_is_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        let name = "documents directory";
        for configured in ["", "../outside", "/absolute", "./data", "data\\back"] {
            let error = resolve(root.path(), [(name, configured)]).unwrap_err();
            assert!(error.to_string().contains(name), "{configured:?}: {error}");
        }
    }

    #[test]
    fn a_directory_is_accepted_only_in_its_normalised_spelling() {
        let root = tempfile::tempdir().unwrap();
        for configured in ["data//notes", "data/notes/", "data/./notes"] {
            assert!(
                resolve(root.path(), [("notes", configured)]).is_err(),
                "{configured:?}"
            );
        }
        let [notes] = resolve(root.path(), [("notes", "data/notes")]).unwrap();
        assert_eq!(notes.relative, "data/notes");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leads_outside_the_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(resolve(root.path(), [("documents", "linked/data")]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_through_a_link_to_nothing_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("missing"), root.path().join("linked"))
            .unwrap();
        for configured in ["linked", "linked/data"] {
            let error = resolve(root.path(), [("documents", configured)]).unwrap_err();
            assert!(
                error.to_string().contains("documents"),
                "{configured}: {error}"
            );
        }
    }

    #[test]
    fn directories_that_differ_only_in_case_overlap() {
        let root = tempfile::tempdir().unwrap();
        for configured in [
            [("upper", "Data"), ("lower", "data")],
            [("upper", "Data/notes"), ("lower", "data")],
        ] {
            let error = resolve(root.path(), configured).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("upper") && message.contains("lower"),
                "{configured:?}: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_reached_through_a_link_is_held_where_the_link_led() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/real")).unwrap();
        std::os::unix::fs::symlink("data/real", root.path().join("alias")).unwrap();
        let [notes] = resolve(root.path(), [("notes", "alias/notes")]).unwrap();
        std::fs::remove_file(root.path().join("alias")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("alias")).unwrap();
        notes.dir.write("held.yaml", b"held").unwrap();
        assert!(
            root.path().join("data/real/notes/held.yaml").is_file(),
            "the held directory does not follow the link"
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn overlapping_directories_are_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        let error =
            resolve(root.path(), [("outer", "data"), ("inner", "data/nested")]).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("outer") && message.contains("inner"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directories_that_a_link_within_the_root_makes_overlap_are_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/nested")).unwrap();
        std::os::unix::fs::symlink("data", root.path().join("alias")).unwrap();
        for configured in [
            [("linked", "alias"), ("direct", "data")],
            [("linked", "alias"), ("direct", "data/nested")],
            [("linked", "alias/missing"), ("direct", "data")],
        ] {
            let error = resolve(root.path(), configured).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("linked") && message.contains("direct"),
                "{configured:?}: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_nested_directory_overlaps_the_directory_holding_its_target() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/nested")).unwrap();
        std::os::unix::fs::symlink("data/nested", root.path().join("alias")).unwrap();
        let error = resolve(root.path(), [("linked", "alias"), ("direct", "data")]).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("linked") && message.contains("direct"),
            "{message}"
        );
        resolve(root.path(), [("linked", "alias"), ("other", "elsewhere")]).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_link_leads_out_of_the_root_whatever_it_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        std::os::unix::fs::symlink(root.path().join("data"), root.path().join("alias")).unwrap();
        let error = resolve(root.path(), [("linked", "alias")]).unwrap_err();
        assert!(error.to_string().contains("linked"), "{error}");
    }

    #[test]
    fn a_directory_name_windows_does_not_allow_is_refused() {
        let root = tempfile::tempdir().unwrap();
        for configured in [
            "con",
            "data/AUX",
            "data/com1/notes",
            "nul.d",
            "media?",
            "media.",
            "media ",
            "data/a:b",
            "x<y",
            "tab\tname",
        ] {
            let error = resolve(root.path(), [("documents", configured)]).unwrap_err();
            assert!(
                error.to_string().contains("documents"),
                "{configured}: {error}"
            );
        }
        resolve(root.path(), [("documents", "data/console")]).unwrap();
        resolve(root.path(), [("documents", ".media/v1.2")]).unwrap();
    }

    #[test]
    fn a_file_where_a_configured_directory_goes_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes"), b"a file").unwrap();
        for configured in ["notes", "notes/inner"] {
            let error = resolve(root.path(), [("documents", configured)]).unwrap_err();
            assert!(
                error.to_string().contains("documents"),
                "{configured}: {error}"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path().join("notes"), root.path().join("linked"))
                .unwrap();
            assert!(resolve(root.path(), [("documents", "linked")]).is_err());
        }
    }

    #[test]
    fn a_directory_spelled_outside_ascii_is_refused() {
        let root = tempfile::tempdir().unwrap();
        for configured in ["caf\u{e9}", "cafe\u{301}", "data/\u{fc}ber"] {
            let error = resolve(root.path(), [("documents", configured)]).unwrap_err();
            assert!(
                error.to_string().contains("documents"),
                "{configured}: {error}"
            );
        }
    }
}
