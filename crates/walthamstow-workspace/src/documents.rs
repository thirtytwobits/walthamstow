//! Copyright (c) Scott A Dixon
//!
//! Finding, reading and reporting on a workspace's authored documents.

use serde::Deserialize;
use std::fs;
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

/// Recursively lists supported YAML and JSON documents beneath a directory.
///
/// Missing directories return `[]`. Results are recursively discovered, filtered to supported
/// document extensions, and sorted lexicographically by path for stable loader behaviour. A
/// link beneath `directory` to a directory or to a document is refused, so discovery never
/// leaves it.
pub fn list_documents(directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "Could not list {}: {error}",
                directory.display()
            ))
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            if path.is_dir() || supported_document(&path) {
                anyhow::bail!(
                    "{} is a link, which workspace discovery does not follow.",
                    path.display()
                );
            }
        } else if kind.is_dir() {
            files.extend(list_documents(&path)?);
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
    files.sort();
    Ok(files)
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
        let listed = list_documents(root.path()).unwrap();
        assert!(listed.iter().all(|path| supported_document(path)));
        assert_eq!(listed.len(), 3);
        assert!(listed.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(list_documents(&root.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn listing_refuses_a_document_that_is_not_a_regular_file_and_passes_over_others() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("kept.yaml"), "").unwrap();
        let _passed =
            std::os::unix::net::UnixListener::bind(root.path().join("notes.sock")).unwrap();
        assert_eq!(
            list_documents(root.path()).unwrap(),
            vec![root.path().join("kept.yaml")]
        );
        let special = root.path().join("special.yaml");
        let _refused = std::os::unix::net::UnixListener::bind(&special).unwrap();
        let error = list_documents(root.path()).unwrap_err();
        assert!(
            error.to_string().contains(&special.display().to_string()),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn listing_refuses_a_link_to_a_directory_or_a_document_and_passes_over_others() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("elsewhere.yaml"), "").unwrap();
        fs::write(outside.path().join("elsewhere.txt"), "").unwrap();
        for (link, target) in [
            ("linked", outside.path().to_path_buf()),
            ("linked.yaml", outside.path().join("elsewhere.yaml")),
            ("nested/linked.json", outside.path().join("elsewhere.yaml")),
        ] {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("kept.yaml"), "").unwrap();
            let link = root.path().join(link);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(&target, &link).unwrap();
            let error = list_documents(root.path()).unwrap_err();
            assert!(
                error.to_string().contains(&link.display().to_string()),
                "{error}"
            );
        }
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("kept.yaml"), "").unwrap();
        symlink(
            outside.path().join("elsewhere.txt"),
            root.path().join("notes.txt"),
        )
        .unwrap();
        assert_eq!(
            list_documents(root.path()).unwrap(),
            vec![root.path().join("kept.yaml")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_listed_is_an_error_not_an_empty_workspace() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("plain"), "").unwrap();
        assert!(list_documents(&root.path().join("plain/documents")).is_err());
        assert!(list_documents(&root.path().join("missing"))
            .unwrap()
            .is_empty());
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
