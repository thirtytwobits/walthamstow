//! Copyright (c) Scott A Dixon
//!
//! The directories a workspace keeps its documents in, each configured relative to its root and
//! held open through the root's handle.

use anyhow::{bail, Context, Result};
use cap_std::fs::Dir;
use std::path::{Component, Path, PathBuf};
use walthamstow_kernel::durability::{create_dir_all_in, WriteDurability};

/// A configured directory, held open. A link within the root changed later does not move it.
#[derive(Debug)]
pub struct ConfiguredDirectory {
    pub dir: Dir,
    /// Where the directory lies relative to the root, every link on the way to it resolved, with
    /// `/` between components.
    pub relative: String,
}

/// How many links the way to a configured directory may pass through.
const MAX_LINKS: usize = 40;

/// Resolves each `(name, configured)` directory through the workspace root `root`, creating each
/// that does not exist as durably as `durability` asks, and gives where each lies with every link
/// within the root resolved. A directory must be a normalised relative path in ASCII, every
/// component a name Windows allows, and no two may overlap, compared case-folded where they are
/// configured and where they lie, since a case-insensitive file system holds two that differ only
/// in case as one. A directory that would leave the root, or where a part of it that exists is
/// not a directory, fails through the root's handle. A refusal names each directory by its
/// `name`.
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
    let mut held = Vec::with_capacity(N);
    for (name, value) in configured {
        let dir = open_or_create(durability, root, name, value)?;
        let relative = resolved_path(root, name, value)?;
        held.push(ConfiguredDirectory { dir, relative });
    }
    let folded = held
        .iter()
        .map(|directory| PathBuf::from(directory.relative.to_lowercase()))
        .collect::<Vec<_>>();
    refuse_overlap(&configured, |left, right| {
        folded[left].starts_with(&folded[right]) || folded[right].starts_with(&folded[left])
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

/// Where `configured` lies beneath `root` with every link on the way resolved, read through the
/// root's handle a component at a time: a link is replaced by its target, which a handle follows
/// only when it is relative, and `..` in a target leaves the directory before it. Each component
/// needs only to be searchable, not listable.
fn resolved_path(root: &Dir, name: &str, configured: &str) -> Result<String> {
    let mut resolved: Vec<String> = Vec::new();
    let mut pending: std::collections::VecDeque<String> =
        configured.split('/').map(str::to_owned).collect();
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        match component.as_str() {
            "" | "." => continue,
            ".." => {
                if resolved.pop().is_none() {
                    bail!("{name} must stay within the workspace root.");
                }
                continue;
            }
            _ => {}
        }
        let here = resolved
            .iter()
            .chain(std::iter::once(&component))
            .collect::<PathBuf>();
        let metadata = root
            .symlink_metadata(&here)
            .with_context(|| format!("Failed to resolve {name}."))?;
        if !metadata.is_symlink() {
            resolved.push(component);
            continue;
        }
        links += 1;
        if links > MAX_LINKS {
            bail!("{name} passes through more than {MAX_LINKS} links.");
        }
        let target = root
            .read_link(&here)
            .with_context(|| format!("{name} must stay within the workspace root."))?;
        let mut followed = Vec::new();
        for part in target.components() {
            let part = part.as_os_str().to_str().ok_or_else(|| {
                anyhow::anyhow!("{name} passes through a link not named in UTF-8.")
            })?;
            followed.push(part.to_owned());
        }
        for part in followed.into_iter().rev() {
            pending.push_front(part);
        }
    }
    if resolved.is_empty() {
        bail!("{name} must lie beneath the workspace root, not be it.");
    }
    Ok(resolved.join("/"))
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
        assert_eq!(notes.relative, "data/real/notes");
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
    fn directories_whose_resolved_places_differ_only_in_case_overlap() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("Data/notes")).unwrap();
        if root.path().join("data").exists() {
            // A file system that folds case holds the two as one, which the identities show.
            return;
        }
        std::fs::create_dir_all(root.path().join("data/notes")).unwrap();
        std::os::unix::fs::symlink("Data", root.path().join("alias")).unwrap();
        let error = resolve(
            root.path(),
            [("linked", "alias/notes"), ("direct", "data/notes")],
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("linked") && message.contains("direct"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_beside_one_reached_through_a_link_need_not_be_readable() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/notes")).unwrap();
        std::fs::create_dir_all(root.path().join("closed/inner")).unwrap();
        std::os::unix::fs::symlink("data/notes", root.path().join("alias")).unwrap();
        let closed = root.path().join("closed");
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let resolved = resolve(root.path(), [("notes", "alias")]);
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let [notes] = resolved.unwrap();
        assert_eq!(notes.relative, "data/notes");
    }

    // Linux resolves through a directory it may only search; macOS opens each directory on the
    // way, which needs it to be readable.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_beneath_one_that_can_be_searched_but_not_listed_resolves() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/real/notes")).unwrap();
        std::os::unix::fs::symlink("real", root.path().join("data/alias")).unwrap();
        let data = root.path().join("data");
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o111)).unwrap();
        let resolved = resolve(root.path(), [("notes", "data/alias/notes")]);
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        let [notes] = resolved.unwrap();
        assert_eq!(notes.relative, "data/real/notes");
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
