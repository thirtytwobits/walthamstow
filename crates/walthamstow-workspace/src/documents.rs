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
/// document extensions, and sorted lexicographically by path for stable loader behaviour.
pub fn list_documents(directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(list_documents(&path)?);
        } else if supported_document(&path) {
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
    Ok(relative.to_string_lossy().replace('\\', "/"))
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
}
