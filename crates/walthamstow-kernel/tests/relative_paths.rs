//! Copyright (c) Scott A Dixon
//!
//! Durable writes to paths relative to the current directory. The test binary holds this one
//! test, because it changes the process's current directory.

use std::path::Path;
use walthamstow_kernel::durability::{create_dir_all_synced, write_synced, WriteDurability};

#[test]
fn durable_writes_take_paths_relative_to_the_current_directory() {
    let started_in = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    for (durability, prefix) in [
        (WriteDurability::Device, "device"),
        (WriteDurability::System, "system"),
    ] {
        let directory = format!("{prefix}-data");
        create_dir_all_synced(durability, Path::new(&directory)).unwrap();
        let record = format!("{prefix}-record");
        write_synced(durability, Path::new(&record), b"top").unwrap();
        let nested = format!("{prefix}-new/nested/record");
        write_synced(durability, Path::new(&nested), b"nested").unwrap();
        assert!(dir.path().join(&directory).is_dir());
        assert_eq!(std::fs::read(dir.path().join(&record)).unwrap(), b"top");
        assert_eq!(std::fs::read(dir.path().join(&nested)).unwrap(), b"nested");
    }
    std::env::set_current_dir(started_in).unwrap();
}
