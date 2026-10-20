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
        self.sync(&File::open(directory)?)
    }
}

/// Writes `bytes` to `path` by a rename, durable as `durability` asks once it
/// returns, with any directory it had to create.
pub fn write_synced(durability: WriteDurability, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let mut missing = Vec::new();
        let mut ancestor = parent;
        while !ancestor.try_exists()? {
            missing.push(ancestor);
            ancestor = ancestor
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Missing directory ancestor."))?;
        }
        std::fs::create_dir_all(parent)?;
        // Persist each new directory entry before relying on files beneath it.
        for directory in missing.into_iter().rev() {
            durability.sync_directory(directory.parent().expect("directory ancestor"))?;
        }
    }
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    durability.sync(&file)?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        durability.sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::WriteDurability;
    use std::io::Write;

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
}
