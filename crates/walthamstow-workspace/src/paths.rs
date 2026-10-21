//! Copyright (c) Scott A Dixon
//!
//! The directories a workspace keeps its documents in, each configured relative to its root.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// Resolves each `(name, configured)` directory against `root_dir`. A directory must be a
/// normalised relative path in ASCII that stays within the root, and no two may overlap,
/// through a link within the root or otherwise. Directories that differ only in case overlap,
/// since a case-insensitive file system holds them as one. ASCII is required because file
/// systems differ in which other spellings they hold as one. A refusal names each directory by
/// its `name`.
pub fn resolve_directories<const N: usize>(
    root_dir: &Path,
    configured: [(&str, &str); N],
) -> Result<[PathBuf; N]> {
    let canonical_root = root_dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve workspace root {}.", root_dir.display()))?;
    let mut resolved = Vec::with_capacity(N);
    let mut targets = Vec::with_capacity(N);
    for (name, value) in configured {
        let (directory, target) = resolve_directory(root_dir, &canonical_root, name, value)?;
        resolved.push(directory);
        targets.push(target);
    }
    let targets = targets
        .iter()
        .map(|target| PathBuf::from(target.to_string_lossy().to_lowercase()))
        .collect::<Vec<_>>();
    for left in 0..N {
        for right in (left + 1)..N {
            if targets[left].starts_with(&targets[right])
                || targets[right].starts_with(&targets[left])
            {
                bail!(
                    "{} and {} must not overlap.",
                    configured[left].0,
                    configured[right].0
                );
            }
        }
    }
    Ok(resolved
        .try_into()
        .expect("one resolved directory for each configured one"))
}

/// Whether Windows can name a directory `component`: no character it reserves, no trailing
/// dot or space, which it strips, and no device name, with or without an extension.
fn windows_names(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or_default();
    !component
        .chars()
        .any(|character| character.is_ascii_control() || "<>:\"|?*".contains(character))
        && !component.ends_with(['.', ' '])
        && !walthamstow_kernel::files::is_windows_device_name(stem)
}

/// The directory `configured` names beneath `root_dir` with every link inside the root on
/// the way to it resolved, so a link changed later does not move it, and where it leads in
/// full.
fn resolve_directory(
    root_dir: &Path,
    canonical_root: &Path,
    name: &str,
    configured: &str,
) -> Result<(PathBuf, PathBuf)> {
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
    if relative
        .components()
        .any(|component| !windows_names(&component.as_os_str().to_string_lossy()))
    {
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

    let resolved = root_dir.join(relative);
    // The deepest entry present, a link whose target is missing included.
    let mut existing = resolved.as_path();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or_else(|| {
                    anyhow!("Failed to resolve {name} within the workspace root.")
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to resolve {name} at {}.", existing.display())
                })
            }
        }
    }
    let canonical_existing = match existing.canonicalize() {
        Ok(canonical) => canonical,
        // A link to nothing cannot be shown to stay within the root.
        Err(_)
            if std::fs::symlink_metadata(existing)
                .is_ok_and(|metadata| metadata.file_type().is_symlink()) =>
        {
            bail!("{name} must stay within the workspace root.")
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to resolve {name} at {}.", existing.display()))
        }
    };
    if !canonical_existing.starts_with(canonical_root) {
        bail!("{name} must stay within the workspace root.");
    }
    if !canonical_existing.is_dir() {
        bail!(
            "{name} must be a directory; {} is not one.",
            existing.display()
        );
    }
    let target = canonical_existing.join(
        resolved
            .strip_prefix(existing)
            .expect("the deepest existing ancestor prefixes the directory"),
    );
    let within_root = root_dir.join(
        target
            .strip_prefix(canonical_root)
            .expect("the target stays within the root"),
    );
    Ok((within_root, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_directory_resolves_beneath_the_root_whether_or_not_it_exists() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/present")).unwrap();
        let configured = [("present", "data/present"), ("absent", "data/absent")];
        let resolved = resolve_directories(root.path(), configured).unwrap();
        for ((_, relative), directory) in configured.iter().zip(&resolved) {
            assert_eq!(directory, &root.path().join(relative));
        }
    }

    #[test]
    fn a_directory_outside_the_root_or_not_normalised_is_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        let name = "documents directory";
        for configured in ["", "../outside", "/absolute", "./data", "data\\back"] {
            let error = resolve_directories(root.path(), [(name, configured)]).unwrap_err();
            assert!(error.to_string().contains(name), "{configured:?}: {error}");
        }
    }

    #[test]
    fn a_directory_is_accepted_only_in_its_normalised_spelling() {
        let root = tempfile::tempdir().unwrap();
        for configured in ["data//notes", "data/notes/", "data/./notes"] {
            assert!(
                resolve_directories(root.path(), [("notes", configured)]).is_err(),
                "{configured:?}"
            );
        }
        let [notes] = resolve_directories(root.path(), [("notes", "data/notes")]).unwrap();
        assert_eq!(notes, root.path().join("data/notes"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leads_outside_the_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(resolve_directories(root.path(), [("documents", "linked/data")]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_through_a_link_to_nothing_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("missing"), root.path().join("linked"))
            .unwrap();
        for configured in ["linked", "linked/data"] {
            let error = resolve_directories(root.path(), [("documents", configured)]).unwrap_err();
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
            let error = resolve_directories(root.path(), configured).unwrap_err();
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
        std::os::unix::fs::symlink(root.path().join("data/real"), root.path().join("alias"))
            .unwrap();
        let [notes] = resolve_directories(root.path(), [("notes", "alias/notes")]).unwrap();
        assert_eq!(notes, root.path().join("data/real/notes"));
        std::fs::remove_file(root.path().join("alias")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("alias")).unwrap();
        assert!(
            notes.starts_with(root.path()),
            "the held directory does not follow the link"
        );
    }

    #[test]
    fn overlapping_directories_are_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        let error = resolve_directories(root.path(), [("outer", "data"), ("inner", "data/nested")])
            .unwrap_err();
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
        std::os::unix::fs::symlink(root.path().join("data"), root.path().join("alias")).unwrap();
        for configured in [
            [("linked", "alias"), ("direct", "data")],
            [("linked", "alias"), ("direct", "data/nested")],
            [("linked", "alias/missing"), ("direct", "data")],
        ] {
            let error = resolve_directories(root.path(), configured).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("linked") && message.contains("direct"),
                "{configured:?}: {message}"
            );
        }
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
            let error = resolve_directories(root.path(), [("documents", configured)]).unwrap_err();
            assert!(
                error.to_string().contains("documents"),
                "{configured}: {error}"
            );
        }
        resolve_directories(root.path(), [("documents", "data/console")]).unwrap();
        resolve_directories(root.path(), [("documents", ".media/v1.2")]).unwrap();
    }

    #[test]
    fn a_file_where_a_configured_directory_goes_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes"), b"a file").unwrap();
        for configured in ["notes", "notes/inner"] {
            let error = resolve_directories(root.path(), [("documents", configured)]).unwrap_err();
            assert!(
                error.to_string().contains("documents"),
                "{configured}: {error}"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path().join("notes"), root.path().join("linked"))
                .unwrap();
            assert!(resolve_directories(root.path(), [("documents", "linked")]).is_err());
        }
    }

    #[test]
    fn a_directory_spelled_outside_ascii_is_refused() {
        let root = tempfile::tempdir().unwrap();
        for configured in ["caf\u{e9}", "cafe\u{301}", "data/\u{fc}ber"] {
            let error = resolve_directories(root.path(), [("documents", configured)]).unwrap_err();
            assert!(
                error.to_string().contains("documents"),
                "{configured}: {error}"
            );
        }
    }
}
