//! Copyright (c) Scott A Dixon
//!
//! The directories a workspace keeps its documents in, each configured relative to its root.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// Resolves each `(name, configured)` directory against `root_dir`. A directory must be a
/// normalised relative path that stays within the root, and no two may overlap.
pub fn resolve_directories<const N: usize>(
    root_dir: &Path,
    configured: [(&str, &str); N],
) -> Result<[PathBuf; N]> {
    let canonical_root = root_dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve workspace root {}.", root_dir.display()))?;
    let mut resolved = Vec::with_capacity(N);
    for (name, value) in configured {
        resolved.push(resolve_directory(root_dir, &canonical_root, name, value)?);
    }
    for left in 0..N {
        for right in (left + 1)..N {
            if resolved[left].starts_with(&resolved[right])
                || resolved[right].starts_with(&resolved[left])
            {
                bail!(
                    "workspace.paths.{} and workspace.paths.{} must not overlap.",
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

fn resolve_directory(
    root_dir: &Path,
    canonical_root: &Path,
    name: &str,
    configured: &str,
) -> Result<PathBuf> {
    if configured.is_empty() || configured.contains('\\') {
        bail!("workspace.paths.{name} must be a non-empty relative path using forward slashes.");
    }
    let relative = Path::new(configured);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("workspace.paths.{name} must stay within the workspace root.");
    }
    let normalised = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if normalised != configured {
        bail!("workspace.paths.{name} must be a normalised relative path.");
    }

    let resolved = root_dir.join(relative);
    let mut existing = resolved.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            anyhow!("Failed to resolve workspace.paths.{name} within the workspace root.")
        })?;
    }
    let canonical_existing = existing.canonicalize().with_context(|| {
        format!(
            "Failed to resolve workspace.paths.{name} at {}.",
            existing.display()
        )
    })?;
    if !canonical_existing.starts_with(canonical_root) {
        bail!("workspace.paths.{name} must stay within the workspace root.");
    }
    Ok(resolved)
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
    fn a_directory_outside_the_root_or_not_normalised_is_refused() {
        let root = tempfile::tempdir().unwrap();
        for configured in ["", "../outside", "/absolute", "./data", "data\\back"] {
            assert!(
                resolve_directories(root.path(), [("documents", configured)]).is_err(),
                "{configured:?}"
            );
        }
    }

    #[test]
    fn a_directory_is_accepted_only_in_its_normalised_spelling() {
        let root = tempfile::tempdir().unwrap();
        for configured in ["data//flows", "data/flows/", "data/./flows"] {
            assert!(
                resolve_directories(root.path(), [("flows", configured)]).is_err(),
                "{configured:?}"
            );
        }
        let [flows] = resolve_directories(root.path(), [("flows", "data/flows")]).unwrap();
        assert_eq!(flows, root.path().join("data/flows"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leads_outside_the_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(resolve_directories(root.path(), [("documents", "linked/data")]).is_err());
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
}
