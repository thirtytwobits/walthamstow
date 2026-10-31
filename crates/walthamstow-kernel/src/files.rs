//! Copyright (c) Scott A Dixon
//!
//! Reading a directory of the engine's files, opening one as a `cap-std` directory, and naming
//! one safely.

use anyhow::{bail, Result};
use cap_std::fs::Dir;
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

/// What one directory is, however a path reached it: through a link, or by a name in another
/// case on a file system that folds case. It is taken from the directory's handle. Unix tells
/// entries apart by device and inode, Windows by the path the handle resolves to, every name in
/// the case it is kept in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity {
    #[cfg(unix)]
    entry: (u64, u64),
    #[cfg(windows)]
    entry: PathBuf,
}

/// The identity of the directory `dir` holds.
pub fn identity(dir: &Dir) -> std::io::Result<Identity> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let metadata = dir.dir_metadata()?;
        Ok(Identity {
            entry: (metadata.dev(), metadata.ino()),
        })
    }
    #[cfg(windows)]
    {
        Ok(Identity {
            entry: winx::file::get_file_path(&dir.try_clone()?.into_std_file())?,
        })
    }
}

/// Whether Windows can name an entry `component`: one name, not empty, at most 255 UTF-16 code
/// units, holding no separator and no character it reserves, with no trailing dot or space,
/// which it strips, and no device name, with or without an extension.
pub fn windows_allows(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or_default();
    !component.is_empty()
        && component.encode_utf16().count() <= 255
        && !component
            .chars()
            .any(|character| character.is_ascii_control() || "<>:\"|?*/\\".contains(character))
        && !component.ends_with(['.', ' '])
        && !is_windows_device_name(stem)
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

/// The directory at `path`, held as a `cap-std` directory, every link in `path` followed. On
/// Windows the handle shares deletion, as every open of a workspace root must while an engine
/// holds it, and holds write access, through which
/// [`WriteDurability::sync_dir`](crate::durability::WriteDurability::sync_dir) flushes it.
pub fn open_dir(path: &Path) -> std::io::Result<Dir> {
    #[cfg(not(windows))]
    {
        Dir::open_ambient_dir(path, cap_std::ambient_authority())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x0000_0007;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ_WRITE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        if !opened.metadata()?.is_dir() {
            return Err(std::io::Error::other(format!(
                "{} is not a directory.",
                path.display()
            )));
        }
        Ok(Dir::from_std_file(opened))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_allows_one_plain_name_and_nothing_else() {
        for refused in [
            "",
            "a/b",
            "a\\b",
            "con",
            "con.txt",
            "COM1.yaml",
            "note.",
            "note ",
            "a:b",
            "what?",
            "tab\tname",
        ] {
            assert!(!windows_allows(refused), "{refused:?}");
        }
        for allowed in ["note.yaml", "console", ".media", "Ünïcode"] {
            assert!(windows_allows(allowed), "{allowed:?}");
        }
        // A name's length is counted in UTF-16 code units, two to each of these.
        assert!(windows_allows(&"😀".repeat(127)));
        assert!(!windows_allows(&"😀".repeat(128)));
        assert!(windows_allows(&"a".repeat(255)));
        assert!(!windows_allows(&"a".repeat(256)));
    }

    #[test]
    fn a_directory_reached_by_another_path_has_one_identity() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("Data");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();
        let of = |path: &Path| identity(&open_dir(path).unwrap()).unwrap();
        assert_eq!(of(&data), of(&root.path().join("other/../Data")));
        assert_ne!(of(&data), of(&root.path().join("other")));
        let folded = root.path().join("data");
        if folded.exists() {
            assert_eq!(of(&data), of(&folded));
        }
        #[cfg(unix)]
        {
            let linked = root.path().join("linked");
            std::os::unix::fs::symlink(&data, &linked).unwrap();
            assert_eq!(of(&data), of(&linked));
        }
    }

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

    #[test]
    fn a_held_workspace_root_opens_and_is_written_and_synced_through_its_dir() {
        use crate::durability::{create_dir_all_in, write_synced_in, WriteDurability};
        let root = tempfile::tempdir().unwrap();
        let _held = crate::workspace_lock::WorkspaceLock::acquire(root.path()).unwrap();
        let dir = open_dir(root.path()).unwrap();
        for durability in [WriteDurability::Device, WriteDurability::System] {
            write_synced_in(durability, &dir, Path::new("top.json"), b"top").unwrap();
            create_dir_all_in(durability, &dir, Path::new("a/b")).unwrap();
            write_synced_in(durability, &dir, Path::new("a/b/c.json"), b"c").unwrap();
            durability.sync_dir(&dir).unwrap();
        }
        assert_eq!(std::fs::read(root.path().join("top.json")).unwrap(), b"top");
        assert_eq!(std::fs::read(root.path().join("a/b/c.json")).unwrap(), b"c");
    }

    #[test]
    fn a_file_is_not_opened_as_a_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plain"), b"x").unwrap();
        assert!(open_dir(&root.path().join("plain")).is_err());
        assert!(open_dir(&root.path().join("missing")).is_err());
    }
}
