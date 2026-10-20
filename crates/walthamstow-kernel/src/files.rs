//! Copyright (c) Scott A Dixon
//!
//! Reading a directory of the engine's files, and naming one safely.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Refuses `id` unless it is safe as one component of a path: not empty, not hidden and
/// holding no separator.
pub fn safe_id(id: &str) -> Result<()> {
    if id.is_empty() || id.starts_with('.') || id.contains(['/', '\\']) {
        bail!("Invalid resource identity.");
    }
    Ok(())
}

/// Every file under `dir`, by its path relative to `dir`. A symlink or any other entry
/// that is neither a file nor a directory is refused.
pub fn read_files(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn visit(base: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                bail!(
                    "{} is a symlink, which is not read as a file.",
                    entry.path().display()
                );
            }
            if kind.is_dir() {
                visit(base, &entry.path(), files)?;
            } else if kind.is_file() {
                files.insert(
                    entry.path().strip_prefix(base)?.to_owned(),
                    std::fs::read(entry.path())?,
                );
            } else {
                bail!(
                    "{} is neither a file nor a directory.",
                    entry.path().display()
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(dir, dir, &mut files)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_is_safe_only_as_one_visible_path_component() {
        assert!(safe_id("a-note").is_ok());
        for unsafe_id in ["", ".hidden", "a/b", "a\\b", "..", "../escape"] {
            assert!(safe_id(unsafe_id).is_err(), "{unsafe_id:?}");
        }
    }

    #[test]
    fn a_directory_reads_as_its_files_by_relative_path_and_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let nested = Path::new("nested/record.json");
        let top = Path::new("top.txt");
        std::fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join(nested), b"nested").unwrap();
        std::fs::write(dir.path().join(top), b"top").unwrap();
        let files = read_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[nested], b"nested");
        assert_eq!(files[top], b"top");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join(top), dir.path().join("link")).unwrap();
            assert!(read_files(dir.path()).is_err());
        }
    }
}
