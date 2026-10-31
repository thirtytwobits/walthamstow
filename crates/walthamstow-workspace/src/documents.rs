//! Copyright (c) Scott A Dixon
//!
//! Finding, reading and reporting on a workspace's authored documents.

use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use walthamstow_kernel::hashing::sha256_hex;

/// Something a workspace load could not use, named by a reason code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceWarning {
    /// A reason code naming what the load could not use.
    pub code: &'static str,
    /// The document the warning concerns, relative to its configured directory.
    pub path: Option<String>,
    pub message: String,
}

impl std::fmt::Display for WorkspaceWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Returns whether a path is a supported workspace document type.
///
/// Only `.yaml`, `.yml`, and `.json` files participate in workspace discovery.
pub fn supported_document(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("yaml" | "yml" | "json")
    )
}

/// Recursively lists supported YAML and JSON documents in `directory`, by their paths relative
/// to it, sorted lexicographically for stable loader behaviour.
///
/// Discovery walks the directory's handle and never follows a link. A link to a document, to a
/// directory, or out of the directory is refused, since documents behind it would go unfound;
/// a link within the directory to any other file, or to nothing, is passed over. A document that
/// is not a regular file is refused.
pub fn list_documents(directory: &Dir) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_documents(directory, directory, Path::new(""), &mut files)?;
    files.sort();
    Ok(files)
}

/// Adds each document in `dir`, which lies at `prefix` in the listed directory `listed`, to
/// `files`. A link is classified through `listed`, so one that stays within it is told apart
/// from one that leaves it wherever it lies.
fn collect_documents(
    listed: &Dir,
    dir: &Dir,
    prefix: &Path,
    files: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let listing =
        |error: std::io::Error| anyhow::anyhow!("Could not list {}: {error}", prefix.display());
    for entry in dir.entries().map_err(listing)? {
        let entry = entry.map_err(listing)?;
        let name = entry.file_name();
        let path = prefix.join(&name);
        let kind = entry.file_type().map_err(listing)?;
        if kind.is_symlink() {
            let leads_elsewhere = match listed.metadata(&path) {
                Ok(metadata) => metadata.is_dir(),
                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
            };
            if leads_elsewhere || supported_document(&path) {
                anyhow::bail!(
                    "{} is a link, which workspace discovery does not follow.",
                    path.display()
                );
            }
        } else if kind.is_dir() {
            let nested = dir.open_dir_nofollow(&name).map_err(listing)?;
            collect_documents(listed, &nested, &path, files)?;
        } else if supported_document(&path) {
            if !kind.is_file() {
                anyhow::bail!(
                    "{} is not a regular file, which workspace discovery does not read.",
                    path.display()
                );
            }
            files.push(path);
        }
    }
    Ok(())
}

/// Converts an absolute file path into a workspace-relative path using forward slashes.
pub fn relative_path(root: &Path, file: &Path) -> anyhow::Result<String> {
    let relative = file.strip_prefix(root).map_err(|error| {
        anyhow::anyhow!(
            "Failed to make {} relative to {}: {error}",
            file.display(),
            root.display()
        )
    })?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("{} does not lie within {}.", file.display(), root.display());
    }
    let relative = relative
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("{} is not named in UTF-8.", file.display()))?;
    Ok(relative.replace('\\', "/"))
}

/// Parses a YAML or JSON document into the requested strongly typed shape.
///
/// Files ending in `.json` are decoded as JSON; every other supported workspace
/// document is decoded as YAML.
pub fn parse_document<T>(path: &Path, text: &str) -> anyhow::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(text).map_err(Into::into),
        _ => serde_yaml::from_str(text).map_err(Into::into),
    }
}

/// Computes a stable content hash for baseline change detection.
///
/// The returned digest is the SHA-256 hex encoding of the exact input bytes.
pub fn hash_text(value: &str) -> String {
    sha256_hex(value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The directory at `path`, held as a `Dir`.
    fn dir(path: &Path) -> Dir {
        walthamstow_kernel::files::open_dir(path).unwrap()
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct SimpleDocument {
        name: String,
    }

    #[test]
    fn relative_path_uses_forward_slashes_and_rejects_paths_outside_the_root() {
        let root = Path::new("/tmp/workspace");
        let file = Path::new("/tmp/workspace/data/notes/first.yaml");
        assert_eq!(
            relative_path(root, file).expect("relative path"),
            "data/notes/first.yaml"
        );

        let outside = Path::new("/tmp/elsewhere/hero.yaml");
        let error = relative_path(root, outside).expect_err("outside root");
        assert!(error.to_string().contains("Failed to make"));
    }

    #[test]
    fn parse_document_dispatches_between_json_and_yaml() {
        let json_doc: SimpleDocument =
            parse_document(Path::new("hero.json"), r#"{"name":"Ariadne"}"#).expect("json");
        let yaml_doc: SimpleDocument =
            parse_document(Path::new("hero.yaml"), "name: Ariadne\n").expect("yaml");

        assert_eq!(
            json_doc,
            SimpleDocument {
                name: "Ariadne".to_string()
            }
        );
        assert_eq!(
            yaml_doc,
            SimpleDocument {
                name: "Ariadne".to_string()
            }
        );

        let error = parse_document::<SimpleDocument>(Path::new("hero.json"), "{")
            .expect_err("invalid json");
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn a_text_hash_tells_texts_apart_and_names_one_text_alike() {
        assert_eq!(hash_text("abc"), hash_text("abc"));
        assert_ne!(hash_text("abc"), hash_text("abcd"));
        assert_eq!(hash_text("abc").len(), 64);
    }

    #[test]
    fn listing_finds_nested_documents_in_order_and_skips_other_files() {
        let root = tempfile::tempdir().unwrap();
        for path in ["b.yaml", "a/c.json", "a/d.yml", "e.txt", "a/f.md"] {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let listed = list_documents(&dir(root.path())).unwrap();
        assert_eq!(listed, ["a/c.json", "a/d.yml", "b.yaml"].map(PathBuf::from));
    }

    #[cfg(unix)]
    #[test]
    fn listing_refuses_a_document_that_is_not_a_regular_file_and_passes_over_others() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("kept.yaml"), "").unwrap();
        let _passed =
            std::os::unix::net::UnixListener::bind(root.path().join("notes.sock")).unwrap();
        assert_eq!(
            list_documents(&dir(root.path())).unwrap(),
            vec![PathBuf::from("kept.yaml")]
        );
        let _refused =
            std::os::unix::net::UnixListener::bind(root.path().join("special.yaml")).unwrap();
        let error = list_documents(&dir(root.path())).unwrap_err();
        assert!(error.to_string().contains("special.yaml"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn listing_refuses_a_link_to_a_directory_a_document_or_out_and_passes_over_others() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("elsewhere.yaml"), "").unwrap();
        fs::write(outside.path().join("elsewhere.txt"), "").unwrap();
        for (link, target) in [
            ("linked", outside.path().to_path_buf()),
            ("linked.yaml", outside.path().join("elsewhere.yaml")),
            ("nested/linked.json", outside.path().join("elsewhere.yaml")),
            // A handle cannot see what a link out of the directory leads to.
            ("notes.txt", outside.path().join("elsewhere.txt")),
        ] {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("kept.yaml"), "").unwrap();
            let path = root.path().join(link);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink(&target, &path).unwrap();
            let error = list_documents(&dir(root.path())).unwrap_err();
            assert!(error.to_string().contains(link), "{error}");
        }
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("kept.yaml"), "").unwrap();
        fs::write(root.path().join("kept.txt"), "").unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        symlink("kept.txt", root.path().join("notes.txt")).unwrap();
        symlink("../kept.txt", root.path().join("nested/notes.txt")).unwrap();
        symlink("missing.txt", root.path().join("dangling.txt")).unwrap();
        assert_eq!(
            list_documents(&dir(root.path())).unwrap(),
            vec![PathBuf::from("kept.yaml")]
        );
    }

    #[test]
    fn a_path_that_leaves_the_root_has_no_relative_path() {
        let root = Path::new("/workspace");
        for file in [
            "/workspace/../secret.yaml",
            "/workspace/data/../../secret.yaml",
        ] {
            assert!(relative_path(root, Path::new(file)).is_err(), "{file}");
        }
        assert!(relative_path(root, Path::new("/workspace/data/world.yaml")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_not_named_in_utf8_has_no_relative_path() {
        use std::os::unix::ffi::OsStrExt;
        let root = Path::new("/workspace");
        let file = root.join(std::ffi::OsStr::from_bytes(b"caf\xe9.json"));
        assert!(relative_path(root, &file).is_err());
    }
}
