//! Copyright (c) Scott A Dixon
//!
//! How far the engine's durable writes go before it relies on them.

use serde::Deserialize;
use std::fs::File;
use std::io;
use std::path::Path;

/// How far a durable write goes before the engine relies on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WriteDurability {
    /// Through to the storage device, so a write survives a loss of power. On
    /// macOS this flushes the drive's cache.
    Device,
    /// To the operating system, so a write survives the engine stopping or
    /// crashing.
    System,
}

impl WriteDurability {
    /// Makes what has been written to `file` durable.
    pub fn sync(self, file: &File) -> io::Result<()> {
        match self {
            Self::Device => file.sync_all(),
            #[cfg(unix)]
            Self::System => rustix::fs::fsync(file).map_err(io::Error::from),
            #[cfg(not(unix))]
            Self::System => file.sync_all(),
        }
    }

    /// Makes the entries of `directory` durable.
    pub fn sync_directory(self, directory: &Path) -> io::Result<()> {
        self.sync(&open_directory(directory)?)
    }
}

/// `directory`, opened so its entries can be synced.
#[cfg(not(windows))]
fn open_directory(directory: &Path) -> io::Result<File> {
    File::open(directory)
}

/// `directory`, opened so its entries can be flushed. Windows opens a directory only with
/// `FILE_FLAG_BACKUP_SEMANTICS`, and flushes only through a handle with write access.
#[cfg(windows)]
fn open_directory(directory: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)
}

/// Creates `directory` and every missing ancestor, each new entry durable as
/// `durability` asks once it returns.
pub fn create_dir_all_synced(durability: WriteDurability, directory: &Path) -> io::Result<()> {
    create_dir_all_with(directory, |entry_of| durability.sync_directory(entry_of))
}

/// Held by each directory creation; see [`create_dir_all_with`].
static CREATING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Creates `directory` and every missing ancestor, calling `sync` on the directory holding
/// each one it creates. When creating or syncing fails, what it created is removed again, so
/// a retry creates it and syncs it once more.
fn create_dir_all_with(
    directory: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    // One creation at a time, so none relies on a directory another is still settling or
    // about to remove again.
    let _creating = CREATING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = present(directory);
    let mut missing = Vec::new();
    let mut ancestor = directory;
    while !ancestor.try_exists()? {
        missing.push(ancestor);
        ancestor =
            parent(ancestor).ok_or_else(|| io::Error::other("Missing directory ancestor."))?;
    }
    if !std::fs::metadata(ancestor)?.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory.",
            ancestor.display()
        )));
    }
    // Created one at a time, so a failure removes only the directories this call made.
    let mut made = Vec::new();
    let created = missing.iter().rev().try_for_each(|missing| {
        match std::fs::create_dir(missing) {
            Ok(()) => made.push(*missing),
            // Another caller made it meanwhile, and may not have synced its entry yet.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        sync(parent(missing).expect("a created directory has a parent"))
    });
    if created.is_err() {
        for made in made.iter().rev() {
            let _ = std::fs::remove_dir(made);
        }
    }
    created
}

/// `path`, with the empty path read as the current directory.
fn present(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

/// The directory holding `path`: the current directory for a relative path of one
/// component.
fn parent(path: &Path) -> Option<&Path> {
    path.parent().map(present)
}

/// Whether `name` is a file name [`write_synced`] stages a write under: the name it writes, its
/// extension replaced by a random UUID and `.tmp`. A stop before the rename leaves it behind.
pub fn staged_by_write(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".tmp") else {
        return false;
    };
    let Some((_, id)) = stem.rsplit_once('.') else {
        return false;
    };
    uuid::Uuid::try_parse(id).is_ok_and(|parsed| {
        parsed.get_version() == Some(uuid::Version::Random)
            && parsed.get_variant() == uuid::Variant::RFC4122
            && parsed.hyphenated().to_string() == id
    })
}

/// Writes `bytes` to `path` by a rename, durable as `durability` asks once it
/// returns, with any directory it had to create. A file it replaces keeps its
/// permissions.
pub fn write_synced(durability: WriteDurability, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(directory) = parent(path) {
        create_dir_all_synced(durability, directory)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let staged = (|| -> io::Result<()> {
        match std::fs::metadata(path) {
            Ok(replaced) => file.set_permissions(replaced.permissions())?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        file.write_all(bytes)?;
        durability.sync(&file)?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    if let Some(directory) = parent(path) {
        durability.sync_directory(directory)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::WriteDurability;

    #[test]
    fn a_staged_name_is_the_written_name_with_a_random_uuid_for_its_extension() {
        let staged = |name: &str| {
            std::path::Path::new(name)
                .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned()
        };
        for written in ["note.yaml", "notes", "a.b.yaml"] {
            assert!(super::staged_by_write(&staged(written)), "{written}");
        }
        for unlike in [
            "note.tmp",
            "note.yaml",
            "note.00000000-0000-0000-0000-000000000000.tmp",
            "note.0B1E2F3A-4C5D-4E6F-8A9B-0C1D2E3F4A5B.tmp",
            "note.0b1e2f3a4c5d4e6f8a9b0c1d2e3f4a5b.tmp",
            "note.0b1e2f3a-4c5d-1e6f-8a9b-0c1d2e3f4a5b.tmp",
        ] {
            assert!(!super::staged_by_write(unlike), "{unlike}");
        }
    }
    use std::io::Write;

    #[test]
    fn each_durability_creates_every_missing_directory_and_keeps_existing_ones() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let dir = tempfile::tempdir().expect("tempdir");
            let nested = dir.path().join("a/b/c");
            super::create_dir_all_synced(durability, &nested).expect("create");
            assert!(nested.is_dir());
            std::fs::write(nested.join("kept"), b"kept").expect("write");
            super::create_dir_all_synced(durability, &nested).expect("create again");
            assert_eq!(std::fs::read(nested.join("kept")).expect("read"), b"kept");
        }
    }

    #[test]
    fn each_durability_keeps_what_was_written_to_a_file_and_a_directory() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("record");
            let written = b"accepted";
            let mut file = std::fs::File::create(&path).expect("create");
            file.write_all(written).expect("write");
            durability.sync(&file).expect("sync file");
            durability
                .sync_directory(dir.path())
                .expect("sync directory");
            assert_eq!(std::fs::read(&path).expect("read"), written);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("record");
        for mode in [0o600, 0o640] {
            std::fs::write(&path, b"before").expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            super::write_synced(WriteDurability::System, &path, b"after").expect("replace");
            assert_eq!(std::fs::read(&path).expect("read"), b"after");
            let kept = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(kept & 0o777, mode, "{mode:o}");
        }
    }

    #[test]
    fn a_creation_that_cannot_be_synced_is_undone_and_made_again_on_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a/b/c");
        let failed =
            super::create_dir_all_with(&nested, |_| Err(std::io::Error::other("the sync failed")));
        assert!(failed.is_err());
        assert!(!dir.path().join("a").exists(), "the creation is undone");

        let mut synced = Vec::new();
        super::create_dir_all_with(&nested, |entry_of| {
            synced.push(entry_of.to_path_buf());
            Ok(())
        })
        .expect("retry");
        assert!(nested.is_dir());
        for created in ["a", "a/b", "a/b/c"] {
            let created = dir.path().join(created);
            assert!(
                synced
                    .iter()
                    .any(|entry_of| Some(entry_of.as_path()) == created.parent()),
                "{} is synced into its directory",
                created.display()
            );
        }
    }

    #[test]
    fn a_write_that_fails_leaves_nothing_staged() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("occupied");
            std::fs::create_dir(&path).expect("a directory where the file goes");
            assert!(super::write_synced(durability, &path, b"bytes").is_err());
            let held = std::fs::read_dir(dir.path())
                .expect("list")
                .map(|entry| entry.expect("entry").file_name())
                .collect::<Vec<_>>();
            assert_eq!(held, [std::ffi::OsString::from("occupied")]);
        }
    }

    #[test]
    fn a_failed_creation_leaves_what_another_caller_made_meanwhile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a/b/c");
        let theirs = dir.path().join("a/b");
        let mut calls = 0;
        let failed = super::create_dir_all_with(&nested, |_| {
            calls += 1;
            if calls == 1 {
                std::fs::create_dir(&theirs).expect("another caller makes a/b");
                Ok(())
            } else {
                Err(std::io::Error::other("the sync failed"))
            }
        });
        assert!(failed.is_err());
        assert!(theirs.is_dir(), "the other caller's directory stays");
        assert!(!nested.exists(), "this call's directory is removed");
    }

    #[test]
    fn a_directory_another_caller_made_meanwhile_is_settled_here_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a/b/c");
        let theirs = dir.path().join("a/b");
        let mut synced = Vec::new();
        super::create_dir_all_with(&nested, |entry_of| {
            if synced.is_empty() {
                std::fs::create_dir(&theirs).expect("another caller makes a/b");
            }
            synced.push(entry_of.to_path_buf());
            Ok(())
        })
        .expect("create");
        assert!(nested.is_dir());
        assert!(
            synced
                .iter()
                .any(|entry_of| Some(entry_of.as_path()) == theirs.parent()),
            "{synced:?}"
        );
    }

    #[test]
    fn a_creation_begun_while_another_settles_waits_for_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("a");
        let second = dir.path().join("a/b");
        let waiting = std::thread::scope(|scope| {
            let mut waiting = None;
            let failed = super::create_dir_all_with(&first, |_| {
                let second = second.clone();
                waiting = Some(scope.spawn(move || {
                    let mut synced = Vec::new();
                    super::create_dir_all_with(&second, |entry_of| {
                        synced.push(entry_of.to_path_buf());
                        Ok(())
                    })
                    .expect("the second creation");
                    synced
                }));
                Err(std::io::Error::other("the first sync failed"))
            });
            assert!(failed.is_err());
            waiting.expect("spawned").join().expect("joined")
        });
        assert!(second.is_dir());
        assert!(
            waiting
                .iter()
                .any(|entry_of| Some(entry_of.as_path()) == first.parent()),
            "the second creation made and settled a itself: {waiting:?}"
        );
    }

    #[test]
    fn a_file_where_a_directory_goes_is_refused() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let dir = tempfile::tempdir().expect("tempdir");
            let file = dir.path().join("plain");
            std::fs::write(&file, b"x").expect("write");
            assert!(super::create_dir_all_synced(durability, &file).is_err());
            assert!(super::create_dir_all_synced(durability, &file.join("below")).is_err());
        }
    }
}
