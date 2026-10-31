//! Copyright (c) Scott A Dixon
//!
//! The conformance suite over the file log store, and the rules only files can break.

use super::*;
use crate::conformance::{Breaker, Harness};
use crate::fault::FaultKind;
use crate::layout::{entity_directory_name, resource_directory_name};
use walthamstow_kernel::local_store::MemoryLocalStore;

/// The entities the conformance suite writes to.
const ENTITIES: [&str; 2] = ["Log", "Other"];
const HEAD: &str = "head.json";
const LOG: &str = "log.ndjson";

/// Stores over scratch directories, which the harness holds while the test runs. Each store it
/// builds has a root and a data directory of its own, so each starts empty.
struct FileHarness {
    scratch: tempfile::TempDir,
    /// The root and data directory of the store built last.
    current: Mutex<(PathBuf, PathBuf)>,
}

impl FileHarness {
    fn new() -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let current = Mutex::new((scratch.path().join("root"), scratch.path().join("data")));
        Self { scratch, current }
    }

    /// A fresh root and data directory, which become the current ones.
    fn fresh(&self) -> (PathBuf, PathBuf) {
        let next = tempfile::tempdir_in(self.scratch.path()).unwrap().keep();
        let dirs = (next.join("root"), next.join("data"));
        std::fs::create_dir_all(&dirs.0).unwrap();
        *locked(&self.current) = dirs.clone();
        dirs
    }

    fn config(
        &self,
        leases: Arc<dyn Leases>,
        clock: WallClock,
        local: Arc<dyn LocalStore>,
    ) -> FileLogConfig {
        let (root, data_dir) = locked(&self.current).clone();
        FileLogConfig {
            layout: Arc::new(
                ENTITIES
                    .into_iter()
                    .fold(DocumentLayout::open(&root).unwrap(), |layout, entity| {
                        layout.with_entity(entity, [HEAD, LOG]).unwrap()
                    }),
            ),
            files: ENTITIES
                .into_iter()
                .map(|entity| (entity.to_owned(), LogFiles::new(HEAD, LOG).unwrap()))
                .collect(),
            durability: WriteDurability::System,
            leases,
            clock,
            local,
            data_dir,
            settled: SETTLED,
        }
    }

    /// The path of `file` in `document`'s directory, in the current root.
    fn path(&self, document: &DocumentId, file: &str) -> PathBuf {
        locked(&self.current)
            .0
            .join(entity_directory_name(document.entity()).unwrap())
            .join(resource_directory_name(document.resource()).unwrap())
            .join(file)
    }
}

impl Harness for FileHarness {
    fn store(&self, leases: Arc<dyn Leases>, clock: WallClock) -> Box<dyn LogStore> {
        self.fresh();
        let local = Arc::new(MemoryLocalStore::new());
        Box::new(
            FileLogStore::open(self.config(leases, clock, local))
                .unwrap()
                .0,
        )
    }
}

impl Breaker for FileHarness {
    fn break_document(&self, document: &DocumentId, kind: FaultKind) {
        let log = self.path(document, LOG);
        match kind {
            FaultKind::Missing => std::fs::remove_file(log).unwrap(),
            FaultKind::Invalid => {
                let mut bytes = std::fs::read(&log).unwrap();
                bytes.extend_from_slice(b"not a record\n");
                std::fs::write(log, bytes).unwrap();
            }
            FaultKind::Inaccessible => {
                std::fs::remove_file(&log).unwrap();
                std::fs::create_dir(log).unwrap();
            }
        }
    }
}

crate::log_store_conformance!(harness = FileHarness::new());
crate::log_store_conformance!(broken = FileHarness::new());

/// The settled age the tests' stores are opened with.
const SETTLED: Duration = Duration::from_secs(2);

mod files {
    //! The rules only files can break: interruptions, edits made outside the store, and the
    //! stamp cache.

    use super::*;
    use crate::blobs::{BlobStore, FileBlobStore};
    use crate::types::NewRecord;
    use crate::MemoryLogStore;
    use clerkenwell_events::{ActorKind, Principal};
    use serde_json::Value;
    use std::time::SystemTime;
    use walthamstow_kernel::clock::Moment;
    use walthamstow_kernel::leases::{LeaseHolder, MemoryLeases};

    /// A file store over the harness's current directories, its leases, and a wall clock the
    /// test moves.
    struct Setup {
        harness: FileHarness,
        leases: Arc<MemoryLeases>,
        wall: Arc<Mutex<SystemTime>>,
        local: Arc<dyn LocalStore>,
        store: FileLogStore,
        writes: std::sync::atomic::AtomicUsize,
    }

    impl Setup {
        fn new() -> Self {
            let harness = FileHarness::new();
            harness.fresh();
            let leases = Arc::new(MemoryLeases::new(Duration::from_secs(60), || {
                Moment::ORIGIN
            }));
            let wall = Arc::new(Mutex::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1)));
            let local: Arc<dyn LocalStore> = Arc::new(MemoryLocalStore::new());
            let store = Self::open(&harness, &leases, &wall, &local).0;
            Self {
                harness,
                leases,
                wall,
                local,
                store,
                writes: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn clock(wall: &Arc<Mutex<SystemTime>>) -> WallClock {
            let wall = wall.clone();
            WallClock::new(move || *locked(&wall))
        }

        fn open(
            harness: &FileHarness,
            leases: &Arc<MemoryLeases>,
            wall: &Arc<Mutex<SystemTime>>,
            local: &Arc<dyn LocalStore>,
        ) -> (FileLogStore, LogRecovery) {
            FileLogStore::open(harness.config(leases.clone(), Self::clock(wall), local.clone()))
                .unwrap()
        }

        /// Opens the store again over the same files and local records, as a restart does.
        fn reopen(&mut self) -> LogRecovery {
            let (store, recovery) =
                Self::open(&self.harness, &self.leases, &self.wall, &self.local);
            self.store = store;
            recovery
        }

        /// Opens the store again with its local records lost.
        fn reopen_without_local_records(&mut self) {
            self.local = Arc::new(MemoryLocalStore::new());
            self.reopen();
        }

        fn lease(&self, document: &DocumentId) -> Lease {
            self.leases
                .acquire(document, &LeaseHolder::new("test"))
                .unwrap()
                .unwrap()
        }

        fn id(&self) -> WriteId {
            let write = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            WriteId::new(format!("write-{write}"))
        }

        fn create(&self, document: &DocumentId) -> Revision {
            let image = LogImage {
                id: self.id(),
                format: 1,
                head: serde_json::json!({ "title": document.resource() }),
                records: Vec::new(),
            };
            self.store.create(&self.lease(document), &image).unwrap()
        }

        fn commit(
            &self,
            document: &DocumentId,
            base: &Revision,
            payloads: &[i64],
        ) -> KernelResult<Revision> {
            let commit = LogCommit {
                id: self.id(),
                head: None,
                records: payloads.iter().map(|payload| record(*payload)).collect(),
            };
            self.store.commit(&self.lease(document), base, &commit)
        }

        fn revision(&self, document: &DocumentId) -> Revision {
            self.store.head(document).unwrap().unwrap().revision
        }

        fn payloads(&self, document: &DocumentId) -> Vec<Value> {
            self.store
                .records(
                    document,
                    &self.revision(document),
                    Position::FIRST,
                    usize::MAX,
                )
                .unwrap()
                .into_iter()
                .map(|record| record.payload)
                .collect()
        }

        fn path(&self, document: &DocumentId, file: &str) -> PathBuf {
            self.harness.path(document, file)
        }

        /// Moves the wall clock to `after` past the moment `document`'s log file last changed.
        fn after_change(&self, document: &DocumentId, after: Duration) {
            let path = self.path(document, LOG);
            let dir = walthamstow_kernel::files::open_dir(path.parent().unwrap()).unwrap();
            let stamp = Stamp::of(&dir.metadata(LOG).unwrap()).unwrap();
            let changed = Duration::from_nanos(u64::try_from(stamp.changed_at()).unwrap());
            *locked(&self.wall) = SystemTime::UNIX_EPOCH + changed + after;
        }

        /// Moves the wall clock until `document`'s log file has settled.
        fn settle(&self, document: &DocumentId) {
            self.after_change(document, SETTLED);
        }

        fn log_reads(&self) -> usize {
            self.store
                .log_reads
                .load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn document(resource: &str) -> DocumentId {
        DocumentId::new("Log", resource)
    }

    fn record(payload: i64) -> NewRecord {
        NewRecord {
            author: Principal::new("writer", ActorKind::Agent),
            blobs: Vec::new(),
            payload: Value::from(payload),
        }
    }

    fn payloads(values: &[i64]) -> Vec<Value> {
        values.iter().copied().map(Value::from).collect()
    }

    /// The fault a read of `document`'s head refuses with.
    fn fault(setup: &Setup, document: &DocumentId) -> LogFault {
        LogFault::of(&setup.store.head(document).unwrap_err()).unwrap()
    }

    /// A document with records `[1, 2]`, and its revision.
    fn with_records(setup: &Setup, resource: &str) -> (DocumentId, Revision) {
        let document = document(resource);
        let created = setup.create(&document);
        let revision = setup.commit(&document, &created, &[1, 2]).unwrap();
        (document, revision)
    }

    #[test]
    fn a_document_is_a_head_file_and_a_log_file_people_can_read() {
        let setup = Setup::new();
        let (document, _) = with_records(&setup, "Turn 1");
        let head: Value =
            serde_json::from_slice(&std::fs::read(setup.path(&document, HEAD)).unwrap()).unwrap();
        assert_eq!(head["format"], 1);
        assert_eq!(head["next_position"], 2);
        assert_eq!(head["head"]["title"], "Turn 1");
        let log = std::fs::read_to_string(setup.path(&document, LOG)).unwrap();
        let lines: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["position"], 1);
        assert_eq!(lines[1]["payload"], 2);
        assert!(setup
            .path(&document, LOG)
            .ends_with("log/%54urn%201/log.ndjson"));
    }

    #[test]
    fn a_partial_last_line_is_read_past_and_dropped_by_the_next_commit() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let log = setup.path(&document, LOG);
        let mut bytes = std::fs::read(&log).unwrap();
        let whole = bytes.clone();
        bytes.extend_from_slice(br#"{"position":2,"au"#);
        std::fs::write(&log, &bytes).unwrap();
        assert_eq!(setup.revision(&document), revision);
        assert_eq!(setup.payloads(&document), payloads(&[1, 2]));
        assert_eq!(std::fs::read(&log).unwrap(), bytes, "a read never writes");
        setup.commit(&document, &revision, &[3]).unwrap();
        assert_eq!(setup.payloads(&document), payloads(&[1, 2, 3]));
        let after = std::fs::read(&log).unwrap();
        assert!(after.starts_with(&whole) && after.ends_with(b"\n"));
        assert_eq!(after.iter().filter(|byte| **byte == b'\n').count(), 3);
    }

    #[test]
    fn a_line_appended_without_the_head_raised_is_dropped_by_the_next_commit() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let log = setup.path(&document, LOG);
        let mut bytes = std::fs::read(&log).unwrap();
        let line = String::from_utf8(bytes.clone())
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .replace(r#""position":1"#, r#""position":2"#)
            .replace(r#""payload":2"#, r#""payload":9"#);
        bytes.extend_from_slice(format!("{line}\n").as_bytes());
        std::fs::write(&log, &bytes).unwrap();
        assert_eq!(setup.revision(&document), revision);
        assert_eq!(setup.payloads(&document), payloads(&[1, 2]));
        setup.commit(&document, &revision, &[3]).unwrap();
        assert_eq!(setup.payloads(&document), payloads(&[1, 2, 3]));
    }

    #[test]
    fn a_line_appended_with_the_head_raised_reads_back_as_a_record() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let log = setup.path(&document, LOG);
        let text = std::fs::read_to_string(&log).unwrap();
        let line = text
            .lines()
            .last()
            .unwrap()
            .replace(r#""position":1"#, r#""position":5"#)
            .replace(r#""payload":2"#, r#""payload":9"#);
        std::fs::write(&log, format!("{text}{line}\n")).unwrap();
        let head = setup.path(&document, HEAD);
        let raised = std::fs::read_to_string(&head)
            .unwrap()
            .replace(r#""next_position": 2"#, r#""next_position": 6"#);
        std::fs::write(&head, raised).unwrap();
        assert_ne!(setup.revision(&document), revision);
        assert_eq!(setup.payloads(&document), payloads(&[1, 2, 9]));
        assert_eq!(
            setup.store.head(&document).unwrap().unwrap().next,
            Position::new(6)
        );
        assert_eq!(
            setup
                .commit(&document, &revision, &[3])
                .unwrap_err()
                .reason_code(),
            Some(reason_codes::LOG_STALE)
        );
    }

    #[test]
    fn a_hand_edited_line_is_read_as_edited() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let log = setup.path(&document, LOG);
        let edited = std::fs::read_to_string(&log)
            .unwrap()
            .replace(r#""payload":2"#, r#""payload":{"edited":true}"#);
        std::fs::write(&log, edited).unwrap();
        let after = setup.revision(&document);
        assert_ne!(after, revision);
        assert_eq!(
            setup.payloads(&document),
            [Value::from(1), serde_json::json!({ "edited": true })]
        );
        setup.commit(&document, &after, &[3]).unwrap();
    }

    #[test]
    fn an_old_copy_restored_restores_its_revision_and_refuses_a_commit_over_the_newer() {
        let setup = Setup::new();
        let (document, old) = with_records(&setup, "a");
        let copy = [HEAD, LOG].map(|file| std::fs::read(setup.path(&document, file)).unwrap());
        let newer = setup.commit(&document, &old, &[3]).unwrap();
        for (file, bytes) in [HEAD, LOG].iter().zip(&copy) {
            std::fs::write(setup.path(&document, file), bytes).unwrap();
        }
        assert_eq!(setup.revision(&document), old);
        assert_eq!(setup.payloads(&document), payloads(&[1, 2]));
        assert_eq!(
            setup
                .commit(&document, &newer, &[4])
                .unwrap_err()
                .reason_code(),
            Some(reason_codes::LOG_STALE)
        );
        setup.commit(&document, &old, &[4]).unwrap();
        assert_eq!(setup.payloads(&document), payloads(&[1, 2, 4]));
    }

    #[test]
    fn a_deleted_file_is_reported_by_name() {
        let setup = Setup::new();
        let (document, _) = with_records(&setup, "a");
        for file in [LOG, HEAD] {
            let path = setup.path(&document, file);
            let bytes = std::fs::read(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert_eq!(
                fault(&setup, &document),
                LogFault::Missing {
                    parts: vec![file.to_owned()]
                }
            );
            std::fs::write(path, bytes).unwrap();
        }
        std::fs::remove_file(setup.path(&document, HEAD)).unwrap();
        std::fs::remove_file(setup.path(&document, LOG)).unwrap();
        assert_eq!(setup.store.head(&document).unwrap(), None);
        assert_eq!(setup.store.list("Log", None, 10).unwrap(), []);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_file_is_refused_unopened() {
        let setup = Setup::new();
        let (document, _) = with_records(&setup, "a");
        let log = setup.path(&document, LOG);
        let kept = log.with_file_name("kept.ndjson");
        std::fs::rename(&log, &kept).unwrap();
        std::os::unix::fs::symlink(&kept, &log).unwrap();
        assert!(matches!(
            fault(&setup, &document),
            LogFault::Inaccessible { .. }
        ));
    }

    #[test]
    fn create_over_a_lone_log_file_is_stale_and_leaves_it() {
        let setup = Setup::new();
        let (document, _) = with_records(&setup, "a");
        std::fs::remove_file(setup.path(&document, HEAD)).unwrap();
        let image = LogImage {
            id: setup.id(),
            format: 1,
            head: Value::Null,
            records: Vec::new(),
        };
        let refused = setup.store.create(&setup.lease(&document), &image);
        assert_eq!(
            refused.unwrap_err().reason_code(),
            Some(reason_codes::LOG_STALE)
        );
        assert!(setup.path(&document, LOG).exists());
    }

    #[test]
    fn the_products_entries_are_left_alone_until_the_document_is_removed() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let notes = setup.path(&document, "notes.md");
        std::fs::write(&notes, b"notes").unwrap();
        let rewound = setup
            .store
            .rewind(
                &setup.lease(&document),
                &revision,
                Position::new(1),
                &LogCommit {
                    id: setup.id(),
                    head: None,
                    records: vec![record(7)],
                },
            )
            .unwrap();
        assert_eq!(setup.payloads(&document), payloads(&[1, 7]));
        assert_eq!(std::fs::read(&notes).unwrap(), b"notes");
        setup
            .store
            .remove(&setup.lease(&document), &rewound, &setup.id())
            .unwrap();
        assert!(!notes.parent().unwrap().exists());
    }

    /// Builds the files `write` leaves on `document`, puts the files it found back, and
    /// authorises publishing the ones it left, as an interrupted write leaves them. Returns the
    /// write's revision, what it left, and the operation authorising it.
    fn interrupted(
        setup: &Setup,
        document: &DocumentId,
        write: impl FnOnce(&Setup) -> Revision,
    ) -> (Revision, [Vec<u8>; 2], String) {
        let read = |file| std::fs::read(setup.path(document, file)).unwrap();
        let found = [read(HEAD), read(LOG)];
        let revision = write(setup);
        let left = [read(HEAD), read(LOG)];
        for (file, bytes) in [HEAD, LOG].iter().zip(&found) {
            std::fs::write(setup.path(document, file), bytes).unwrap();
        }
        let changes = [(LOG, &found[1], &left[1]), (HEAD, &found[0], &left[0])].map(
            |(file, source, target)| Change {
                name: file.to_owned(),
                source: Some(sha256_hex(source)),
                target: Some(target.clone()),
            },
        );
        let operation = setup
            .store
            .operations
            .authorise(document, &changes, false)
            .unwrap();
        (revision, left, operation)
    }

    fn rewind(setup: &Setup, document: &DocumentId, base: &Revision) -> Revision {
        setup
            .store
            .rewind(
                &setup.lease(document),
                base,
                Position::new(1),
                &LogCommit {
                    id: setup.id(),
                    head: None,
                    records: vec![record(7)],
                },
            )
            .unwrap()
    }

    #[test]
    fn an_authorised_staged_rewind_is_finished_by_recovery_wherever_it_stopped() {
        for published in [0, 1] {
            let mut setup = Setup::new();
            let (document, revision) = with_records(&setup, "a");
            let (rewound, left, _) = interrupted(&setup, &document, |setup| {
                rewind(setup, &document, &revision)
            });
            if published == 1 {
                std::fs::write(setup.path(&document, LOG), &left[1]).unwrap();
            }
            let recovery = setup.reopen();
            assert_eq!(
                recovery.finished,
                std::slice::from_ref(&document),
                "{published}"
            );
            assert_eq!(setup.revision(&document), rewound);
            assert_eq!(setup.payloads(&document), payloads(&[1, 7]));
            assert_eq!(setup.reopen(), LogRecovery::default());
        }
    }

    #[test]
    fn staging_no_record_authorises_is_discarded() {
        let mut setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let staged = super::operations::STAGING
            .root(&locked(&setup.harness.current).1)
            .join("unauthorised");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join(LOG), b"staged").unwrap();
        let recovery = setup.reopen();
        assert_eq!(recovery.discarded, ["unauthorised"]);
        assert!(recovery.finished.is_empty());
        assert!(!staged.exists());
        assert_eq!(setup.revision(&document), revision);
    }

    #[test]
    fn an_authorisation_whose_file_changed_after_is_discarded_and_the_edit_kept() {
        let mut setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        interrupted(&setup, &document, |setup| {
            rewind(setup, &document, &revision)
        });
        let log = setup.path(&document, LOG);
        let edited = std::fs::read_to_string(&log)
            .unwrap()
            .replace(r#""payload":2"#, r#""payload":5"#);
        std::fs::write(&log, &edited).unwrap();
        let recovery = setup.reopen();
        assert_eq!(recovery.stale, std::slice::from_ref(&document));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), edited);
        assert_eq!(setup.payloads(&document), payloads(&[1, 5]));
    }

    #[test]
    fn a_removal_stopped_midway_is_finished_by_recovery() {
        let mut setup = Setup::new();
        let (document, _) = with_records(&setup, "a");
        std::fs::write(setup.path(&document, "notes.md"), b"notes").unwrap();
        let changes = [HEAD, LOG].map(|file| Change {
            name: file.to_owned(),
            source: Some(sha256_hex(
                &std::fs::read(setup.path(&document, file)).unwrap(),
            )),
            target: None,
        });
        setup
            .store
            .operations
            .authorise(&document, &changes, true)
            .unwrap();
        std::fs::remove_file(setup.path(&document, HEAD)).unwrap();
        let recovery = setup.reopen();
        assert_eq!(recovery.finished, std::slice::from_ref(&document));
        assert!(!setup.path(&document, HEAD).parent().unwrap().exists());
        assert_eq!(setup.store.head(&document).unwrap(), None);
    }

    /// A rewind of `document` from position 1 over `base`, appending 7, as the write `id`.
    fn rewind_as(
        setup: &Setup,
        document: &DocumentId,
        base: &Revision,
        id: &str,
    ) -> KernelResult<Revision> {
        setup.store.rewind(
            &setup.lease(document),
            base,
            Position::new(1),
            &LogCommit {
                id: WriteId::new(id),
                head: None,
                records: vec![record(7)],
            },
        )
    }

    #[test]
    fn a_write_that_leaves_a_publication_authorised_stops_the_document_until_recovery() {
        let mut setup = Setup::new();
        let (document, base) = with_records(&setup, "a");
        let (rewound, _, _) = interrupted(&setup, &document, |setup| {
            rewind_as(setup, &document, &base, "rewind").unwrap()
        });
        locked(&setup.store.stopped).insert(document.clone());
        let refused = setup.commit(&document, &base, &[3]).unwrap_err();
        assert_eq!(refused.code, "internal_error");
        assert_eq!(setup.revision(&document), base, "reads go on");
        let (other, other_base) = with_records(&setup, "b");
        setup.commit(&other, &other_base, &[3]).unwrap();
        let recovery = setup.reopen();
        assert_eq!(recovery.finished, std::slice::from_ref(&document));
        assert_eq!(setup.revision(&document), rewound);
        setup.commit(&document, &rewound, &[3]).unwrap();
        assert_eq!(setup.payloads(&document), payloads(&[1, 7, 3]));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_document_directory_is_listed_and_verified_and_one_leaving_the_root_is_broken() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let entity = setup
            .path(&document, HEAD)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink("a", entity.join("b")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), entity.join("c")).unwrap();
        let states = |listed: Vec<Listed>| -> Vec<(String, Result<Revision, FaultKind>)> {
            listed
                .into_iter()
                .map(|listed| {
                    (
                        listed.document.resource().to_owned(),
                        listed.state.map_err(|fault| fault.kind()),
                    )
                })
                .collect()
        };
        let expected = vec![
            ("a".to_owned(), Ok(revision.clone())),
            ("b".to_owned(), Ok(revision)),
            ("c".to_owned(), Err(FaultKind::Inaccessible)),
        ];
        assert_eq!(states(setup.store.list("Log", None, 10).unwrap()), expected);
        assert_eq!(states(setup.store.verify("Log").unwrap()), expected);
    }

    #[cfg(unix)]
    #[test]
    fn a_document_directory_storage_refuses_is_inaccessible() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let entity = setup
            .path(&document, HEAD)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::remove_dir_all(&entity).unwrap();
        std::fs::write(&entity, b"not a directory").unwrap();
        assert_eq!(fault(&setup, &document).kind(), FaultKind::Inaccessible);
        let records = setup
            .store
            .records(&document, &revision, Position::FIRST, 10)
            .unwrap_err();
        assert_eq!(
            LogFault::of(&records).map(|fault| fault.kind()),
            Some(FaultKind::Inaccessible)
        );
        let unnamed = setup.store.head(&DocumentId::new("Log", "")).unwrap_err();
        assert_eq!(unnamed.code, "invalid_request");
    }

    #[test]
    fn a_removal_whose_directory_is_already_gone_is_finished_by_recovery() {
        let mut setup = Setup::new();
        let (document, _) = with_records(&setup, "a");
        let changes = [HEAD, LOG].map(|file| Change {
            name: file.to_owned(),
            source: Some(sha256_hex(
                &std::fs::read(setup.path(&document, file)).unwrap(),
            )),
            target: None,
        });
        setup
            .store
            .operations
            .authorise(&document, &changes, true)
            .unwrap();
        std::fs::remove_dir_all(setup.path(&document, HEAD).parent().unwrap()).unwrap();
        let recovery = setup.reopen();
        assert_eq!(recovery.finished, std::slice::from_ref(&document));
        assert_eq!(setup.store.head(&document).unwrap(), None);
        assert_eq!(setup.reopen(), LogRecovery::default());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_refuses_a_linked_operation_staging_and_leaves_both_sides_alone() {
        let setup = Setup::new();
        let (document, base) = with_records(&setup, "a");
        let (_, _, operation) = interrupted(&setup, &document, |setup| {
            rewind_as(setup, &document, &base, "rewind").unwrap()
        });
        let staged = super::operations::STAGING
            .root(&locked(&setup.harness.current).1)
            .join(&operation);
        let outside = tempfile::tempdir().unwrap();
        let moved = outside.path().join("staged");
        std::fs::rename(&staged, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &staged).unwrap();
        let before = [HEAD, LOG].map(|file| std::fs::read(setup.path(&document, file)).unwrap());
        assert!(FileLogStore::open(setup.harness.config(
            setup.leases.clone(),
            Setup::clock(&setup.wall),
            setup.local.clone(),
        ))
        .is_err());
        let after = [HEAD, LOG].map(|file| std::fs::read(setup.path(&document, file)).unwrap());
        assert_eq!(after, before);
        assert!(moved.join(LOG).exists());
    }

    #[test]
    fn a_retry_of_another_kind_is_stale_over_restored_files() {
        let setup = Setup::new();
        let (document, base) = with_records(&setup, "a");
        let lease = setup.lease(&document);
        let copy = [HEAD, LOG].map(|file| std::fs::read(setup.path(&document, file)).unwrap());
        let restore = || {
            std::fs::create_dir_all(setup.path(&document, HEAD).parent().unwrap()).unwrap();
            for (file, bytes) in [HEAD, LOG].iter().zip(&copy) {
                std::fs::write(setup.path(&document, file), bytes).unwrap();
            }
        };
        let commit = LogCommit {
            id: WriteId::new("crossed"),
            head: None,
            records: vec![record(3)],
        };
        setup.store.commit(&lease, &base, &commit).unwrap();
        restore();
        let removal = setup.store.remove(&lease, &base, &WriteId::new("crossed"));
        assert_eq!(
            removal.unwrap_err().reason_code(),
            Some(reason_codes::LOG_STALE)
        );
        assert_eq!(setup.revision(&document), base);

        setup
            .store
            .remove(&lease, &base, &WriteId::new("removed"))
            .unwrap();
        restore();
        let commit = LogCommit {
            id: WriteId::new("removed"),
            ..commit
        };
        assert_eq!(
            setup
                .store
                .commit(&lease, &base, &commit)
                .unwrap_err()
                .reason_code(),
            Some(reason_codes::LOG_STALE)
        );
        assert_eq!(setup.revision(&document), base);
    }

    #[cfg(unix)]
    #[test]
    fn a_retried_commit_over_a_read_only_head_file_is_given_its_revision() {
        use std::os::unix::fs::PermissionsExt;
        let setup = Setup::new();
        let (document, base) = with_records(&setup, "a");
        let head = setup.path(&document, HEAD);
        std::fs::set_permissions(&head, std::fs::Permissions::from_mode(0o444)).unwrap();
        let lease = setup.lease(&document);
        let commit = LogCommit {
            id: WriteId::new("read-only"),
            head: None,
            records: vec![record(3)],
        };
        let given = setup.store.commit(&lease, &base, &commit).unwrap();
        assert_eq!(
            std::fs::metadata(&head).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert_eq!(setup.store.commit(&lease, &base, &commit).unwrap(), given);
    }

    #[test]
    fn an_unsettled_stamp_is_hashed_and_a_settled_one_is_trusted() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        let reads = setup.log_reads();
        setup.revision(&document);
        setup.revision(&document);
        assert_eq!(
            setup.log_reads(),
            reads + 2,
            "the clock is before the file changed"
        );
        setup.after_change(&document, SETTLED / 2);
        setup.revision(&document);
        setup.revision(&document);
        assert_eq!(setup.log_reads(), reads + 4, "the file has not settled");
        setup.settle(&document);
        assert_eq!(setup.revision(&document), revision);
        assert_eq!(
            setup.log_reads(),
            reads + 5,
            "the read that records the stamp hashes"
        );
        assert_eq!(setup.revision(&document), revision);
        assert_eq!(
            setup.store.list("Log", None, 10).unwrap()[0].state,
            Ok(revision)
        );
        assert_eq!(setup.log_reads(), reads + 5, "a settled stamp vouches");
    }

    #[test]
    fn a_file_rewritten_within_the_settled_age_is_caught() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        setup.after_change(&document, SETTLED / 2);
        assert_eq!(setup.revision(&document), revision);
        let log = setup.path(&document, LOG);
        let modified = std::fs::metadata(&log).unwrap().modified().unwrap();
        let rewritten = std::fs::read_to_string(&log)
            .unwrap()
            .replace(r#""payload":2"#, r#""payload":5"#);
        std::fs::write(&log, rewritten).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&log)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        setup.settle(&document);
        assert_ne!(setup.revision(&document), revision);
        assert_eq!(setup.payloads(&document), payloads(&[1, 5]));
    }

    #[test]
    fn list_after_the_local_records_are_lost_hashes_every_document_and_agrees() {
        let mut setup = Setup::new();
        for resource in ["a", "b", "c"] {
            with_records(&setup, resource);
        }
        setup.settle(&document("c"));
        let listed = setup.store.list("Log", None, 10).unwrap();
        let reads = setup.log_reads();
        assert_eq!(setup.store.list("Log", None, 10).unwrap(), listed);
        assert_eq!(setup.log_reads(), reads, "the stamps vouch");
        setup.reopen_without_local_records();
        assert_eq!(setup.store.list("Log", None, 10).unwrap(), listed);
        assert_eq!(setup.log_reads(), 3);
    }

    #[test]
    fn a_commit_whose_stamp_hits_does_not_read_the_log_and_gives_the_hashed_revision() {
        let mut setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        setup.settle(&document);
        setup.revision(&document);
        let reads = setup.log_reads();
        let committed = setup.commit(&document, &revision, &[3]).unwrap();
        assert_eq!(setup.log_reads(), reads);
        setup.reopen_without_local_records();
        assert_eq!(setup.revision(&document), committed);
        assert_eq!(setup.payloads(&document), payloads(&[1, 2, 3]));
    }

    #[test]
    fn verify_hashes_every_document_and_records_the_stamps_of_settled_files() {
        let setup = Setup::new();
        let (document, revision) = with_records(&setup, "a");
        setup.settle(&document);
        let reads = setup.log_reads();
        let verified = setup.store.verify("Log").unwrap();
        assert_eq!(verified.len(), 1);
        assert_eq!(verified[0].state, Ok(revision.clone()));
        assert_eq!(setup.log_reads(), reads + 1);
        assert_eq!(setup.revision(&document), revision);
        assert_eq!(setup.log_reads(), reads + 1);
    }

    #[test]
    fn a_round_trip_agrees_with_the_memory_store_and_collects_its_blobs() {
        let setup = Setup::new();
        let memory = MemoryLogStore::new(setup.leases.clone(), Setup::clock(&setup.wall));
        let blobs = FileBlobStore::new(
            setup.store.layout.clone(),
            WriteDurability::System,
            setup.leases.clone(),
            crate::conformance::blobs::kinds(),
        )
        .unwrap();
        let document = document("a");
        let lease = setup.lease(&document);
        let blob = blobs.put(&lease, "capture", b"pixels").unwrap();
        let stores: [&dyn LogStore; 2] = [&setup.store, &memory];
        let same = |revisions: &[Revision; 2]| {
            let [file, kept] = [0, 1].map(|index| {
                let head = stores[index].head(&document).unwrap().unwrap();
                let records = stores[index]
                    .records(&document, &revisions[index], Position::FIRST, usize::MAX)
                    .unwrap();
                (head.format, head.head, head.next, records)
            });
            assert_eq!(file, kept);
        };
        let each = |write: &dyn Fn(&dyn LogStore, &Revision) -> Revision,
                    revisions: &[Revision; 2]| {
            let next = [0, 1].map(|index| write(stores[index], &revisions[index]));
            same(&next);
            next
        };
        let image = |id: &str, records: Vec<StoredRecord>| LogImage {
            id: WriteId::new(id),
            format: 2,
            head: serde_json::json!({ "blob": blob.to_string() }),
            records,
        };
        let created = [0, 1].map(|index| {
            stores[index]
                .create(&lease, &image("create", Vec::new()))
                .unwrap()
        });
        same(&created);
        let committed = each(
            &|store, base| {
                store
                    .commit(
                        &lease,
                        base,
                        &LogCommit {
                            id: WriteId::new("commit"),
                            head: Some(serde_json::json!({ "turn": 1 })),
                            records: vec![record(1), record(2), record(3)],
                        },
                    )
                    .unwrap()
            },
            &created,
        );
        let rewound = each(
            &|store, base| {
                store
                    .rewind(
                        &lease,
                        base,
                        Position::new(1),
                        &LogCommit {
                            id: WriteId::new("rewind"),
                            head: None,
                            records: vec![record(4)],
                        },
                    )
                    .unwrap()
            },
            &committed,
        );
        let records = memory
            .records(&document, &rewound[1], Position::FIRST, 1)
            .unwrap();
        let replaced = each(
            &|store, base| {
                store
                    .replace(&lease, base, &image("replace", records.clone()))
                    .unwrap()
            },
            &rewound,
        );
        for (index, store) in stores.iter().enumerate() {
            store
                .remove(&lease, &replaced[index], &WriteId::new("remove"))
                .unwrap();
            assert_eq!(store.head(&document).unwrap(), None);
        }
        assert_eq!(blobs.get(&document, &blob).unwrap(), None);
        assert_eq!(blobs.list(&document, None, 10).unwrap(), []);
    }

    #[test]
    fn log_files_are_two_files_a_durable_write_does_not_stage_under() {
        assert!(LogFiles::new("head.json", "log.ndjson").is_ok());
        for (head, log) in [
            ("head.json", "HEAD.json"),
            ("head.json", ""),
            ("con", "log.ndjson"),
            ("a/b", "log.ndjson"),
            ("head.json", "log.0b1e2f3a-4c5d-4e6f-8a9b-0c1d2e3f4a5b.tmp"),
        ] {
            assert!(LogFiles::new(head, log).is_err(), "{head:?} {log:?}");
        }
    }

    #[test]
    fn an_entity_whose_files_the_layout_does_not_register_is_refused() {
        let harness = FileHarness::new();
        harness.fresh();
        let leases: Arc<dyn Leases> = Arc::new(MemoryLeases::new(Duration::from_secs(60), || {
            Moment::ORIGIN
        }));
        let mut config = harness.config(
            leases,
            WallClock::new(|| SystemTime::UNIX_EPOCH),
            Arc::new(MemoryLocalStore::new()),
        );
        config.files.insert(
            "Log".into(),
            LogFiles::new("head.json", "other.ndjson").unwrap(),
        );
        assert!(FileLogStore::open(config).is_err());
    }
}
