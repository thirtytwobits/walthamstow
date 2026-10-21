//! Copyright (c) Scott A Dixon
//!
//! Reading a directory of the engine's files, and naming one safely.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Refuses `id` unless it is safe as one component of a path: not empty, not hidden,
/// holding no separator, and read by the platform as exactly one plain name, not a drive
/// or other prefix.
pub fn safe_id(id: &str) -> Result<()> {
    let mut components = Path::new(id).components();
    let one_name = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(name)), None) if name == id
    );
    if id.is_empty() || id.starts_with('.') || id.contains(['/', '\\']) || !one_name {
        bail!("Invalid resource identity.");
    }
    Ok(())
}

/// Whether `name` is a device name Windows reserves in every directory, in any case: CON, PRN,
/// AUX, NUL, and COM or LPT followed by a digit from 1 to 9 or a superscript ¹, ² or ³.
pub const fn is_windows_device_name(name: &str) -> bool {
    const fn named(name: &[u8], device: &[u8]) -> bool {
        if name.len() != device.len() {
            return false;
        }
        let mut index = 0;
        while index < name.len() {
            if name[index].to_ascii_lowercase() != device[index] {
                return false;
            }
            index += 1;
        }
        true
    }
    let name = name.as_bytes();
    if named(name, b"con") || named(name, b"prn") || named(name, b"aux") || named(name, b"nul") {
        return true;
    }
    let numbered = match name.len() {
        4 => matches!(name[3], b'1'..=b'9'),
        // ¹, ² and ³ in UTF-8.
        5 => name[3] == 0xc2 && matches!(name[4], 0xb9 | 0xb2 | 0xb3),
        _ => false,
    };
    if !numbered {
        return false;
    }
    let stem = [name[0], name[1], name[2]];
    named(&stem, b"com") || named(&stem, b"lpt")
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

    #[cfg(windows)]
    #[test]
    fn an_identity_the_platform_reads_as_a_drive_is_refused() {
        for drive in ["C:escape", "C:"] {
            assert!(safe_id(drive).is_err(), "{drive:?}");
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

    #[test]
    fn a_windows_device_name_is_recognised_in_any_case_and_nothing_else_is() {
        for device in [
            "con", "PRN", "Aux", "nul", "COM1", "com9", "Lpt1", "lpt9", "COM¹", "com²", "LPT³",
        ] {
            assert!(is_windows_device_name(device), "{device}");
        }
        for ordinary in [
            "console", "com", "com0", "com10", "lpt", "null", "", "co", "com⁴", "lpt¹0", "comé",
        ] {
            assert!(!is_windows_device_name(ordinary), "{ordinary}");
        }
    }
}
