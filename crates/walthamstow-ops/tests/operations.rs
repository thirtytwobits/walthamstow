//! Copyright (c) Scott A Dixon
//!
//! Walthamstow's operations, called through Clapham's shared runner over the memory adapters
//! and over a workspace's files. Every behaviour runs against both.

use clapham::{Bounds, Cancellation, Category, ComposedSet, OAuthContext, RecordSink, Refusal};
use serde_json::{json, Value};
use std::any::Any;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use walthamstow_kernel::durability::{write_synced, WriteDurability};
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::local_store::{FileLocalStore, JournalKey, LocalStore, MemoryLocalStore};
use walthamstow_kernel::workspace_lock::WorkspaceLock;
use walthamstow_ops::operations::{JOURNALS_READ, WORKSPACE_ADMIN, WORKSPACE_READ};
use walthamstow_ops::{
    codes, Entity, FileWorkspace, FormatStatus, Lease, MemoryLock, MemoryWorkspace, WalthamstowOps,
    Workspace,
};

const DURABILITY: WriteDurability = WriteDurability::System;
const UPGRADE: &str = "format-2";
const PRODUCT_JOURNAL: Journal = Journal::new("product-records");

/// A note's format, named by its `format` field: 2 is current and 1 upgrades to it.
fn note_format(_path: &str, bytes: &[u8]) -> FormatStatus {
    let Ok(note) = serde_json::from_slice::<Value>(bytes) else {
        return FormatStatus::Unreadable {
            message: "not JSON".into(),
        };
    };
    match note["format"].as_u64() {
        Some(1) => FormatStatus::Upgradable {
            upgrade: UPGRADE.into(),
        },
        Some(2) => FormatStatus::Current,
        Some(newer) => FormatStatus::Newer {
            format: newer.to_string(),
        },
        None => FormatStatus::Unreadable {
            message: "no format".into(),
        },
    }
}

fn to_format_2(_entity: &str, _path: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut note: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    note["format"] = json!(2);
    Ok(serde_json::to_vec(&note).expect("a note encodes"))
}

fn note(format: u64, text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "format": format, "text": text })).unwrap()
}

/// Writes a document as a person editing the workspace would.
type Write = Box<dyn Fn(&str, &[u8])>;
/// Reads a document as it is in the workspace.
type Read = Box<dyn Fn(&str) -> Option<Vec<u8>>>;

/// A workspace, its operations composed, and the means to change it behind their back.
struct Rig {
    set: ComposedSet,
    root: String,
    store: Arc<dyn LocalStore>,
    write: Write,
    read: Read,
    /// Holds the workspace as another engine would, until what it returns is dropped.
    hold_elsewhere: Box<dyn Fn() -> Box<dyn Any>>,
    _directories: Vec<tempfile::TempDir>,
}

fn compose(lease: Lease, workspace: Arc<dyn Workspace>, store: Arc<dyn LocalStore>) -> ComposedSet {
    let ops = WalthamstowOps::new(lease, workspace, store)
        .entity(Entity::new("note", "notes", note_format))
        .upgrade(UPGRADE, to_format_2)
        .unwrap()
        .journal(PRODUCT_JOURNAL);
    clapham::compose([ops.contribution()], Bounds::default()).unwrap()
}

fn memory() -> Rig {
    let workspace = Arc::new(MemoryWorkspace::new("memory").directory("note", "notes"));
    let store: Arc<dyn LocalStore> = Arc::new(MemoryLocalStore::new());
    let lock = MemoryLock::new();
    let (written, read) = (workspace.clone(), workspace.clone());
    Rig {
        set: compose(
            Lease::in_process_memory(lock.clone()),
            workspace,
            store.clone(),
        ),
        root: "memory".into(),
        store,
        write: Box::new(move |path, bytes| written.write(path, bytes)),
        read: Box::new(move |path| read.read(path)),
        hold_elsewhere: Box::new(move || Box::new(lock.acquire().unwrap())),
        _directories: Vec::new(),
    }
}

fn files() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let [notes] =
        walthamstow_workspace::paths::resolve_directories(root.path(), [("notes", "notes")])
            .unwrap();
    let workspace =
        Arc::new(FileWorkspace::new(root.path(), data.path(), DURABILITY).directory("note", notes));
    let store: Arc<dyn LocalStore> = FileLocalStore::open(data.path(), DURABILITY).unwrap();
    let path = root.path().to_path_buf();
    let (written, read, held) = (path.clone(), path.clone(), path.clone());
    Rig {
        set: compose(Lease::in_process(&path), workspace, store.clone()),
        root: path.display().to_string(),
        store,
        write: Box::new(move |relative, bytes| {
            write_synced(DURABILITY, &written.join(relative), bytes).unwrap()
        }),
        read: Box::new(move |relative| std::fs::read(read.join(relative)).ok()),
        hold_elsewhere: Box::new(move || Box::new(WorkspaceLock::acquire(&held).unwrap())),
        _directories: vec![root, data],
    }
}

fn rigs() -> [Rig; 2] {
    [memory(), files()]
}

fn grant(scopes: &[&str]) -> OAuthContext {
    OAuthContext::from_verified(
        "https://issuer.test",
        "walthamstow",
        "operator",
        scopes.iter().copied(),
    )
}

fn operator() -> OAuthContext {
    grant(&[WORKSPACE_READ, WORKSPACE_ADMIN, JOURNALS_READ])
}

impl Rig {
    async fn call_as(
        &self,
        auth: OAuthContext,
        name: &str,
        input: Value,
        preview: bool,
    ) -> Result<Value, Refusal> {
        let outcome = self
            .set
            .call(
                &self.set.all(),
                auth,
                name,
                input,
                RecordSink::default(),
                Cancellation::new(),
                preview,
            )
            .await?;
        Ok(outcome.result.expect("a structured result"))
    }

    async fn call(&self, name: &str, input: Value) -> Result<Value, Refusal> {
        self.call_as(operator(), name, input, false).await
    }

    async fn preview(&self, name: &str, input: Value) -> Result<Value, Refusal> {
        self.call_as(operator(), name, input, true).await
    }

    fn seed(&self) {
        (self.write)("notes/old.json", &note(1, "old"));
        (self.write)("notes/current.json", &note(2, "current"));
        (self.write)("notes/broken.json", b"not json");
        (self.write)("notes/ignored.txt", b"not a document");
    }
}

#[tokio::test]
async fn status_reports_the_root_its_holder_and_each_entity_s_documents() {
    for rig in rigs() {
        rig.seed();
        let status = rig
            .call("walthamstow.workspace.status", json!({}))
            .await
            .unwrap();
        assert_eq!(
            status,
            json!({
                "root": rig.root,
                "holder": "process",
                "entities": [{ "name": "note", "directory": "notes", "documents": 3 }],
            })
        );
    }
}

#[tokio::test]
async fn documents_reports_each_typed_document_s_format() {
    for rig in rigs() {
        rig.seed();
        let documents = rig
            .call("walthamstow.workspace.documents", json!({}))
            .await
            .unwrap();
        assert_eq!(
            documents,
            json!({ "documents": [
                {
                    "entity": "note",
                    "path": "notes/broken.json",
                    "format": { "status": "unreadable", "message": "not JSON" },
                },
                { "entity": "note", "path": "notes/current.json", "format": { "status": "current" } },
                {
                    "entity": "note",
                    "path": "notes/old.json",
                    "format": { "status": "upgradable", "upgrade": UPGRADE },
                },
            ]})
        );
    }
}

#[tokio::test]
async fn an_upgrade_previews_then_replaces_only_the_documents_it_applies_to() {
    for rig in rigs() {
        rig.seed();
        let upgrade = "walthamstow.workspace.upgrade";
        let preview = rig
            .preview(upgrade, json!({ "upgrade": UPGRADE }))
            .await
            .unwrap();
        assert_eq!(preview["changed"], json!(["notes/old.json"]));
        assert!(preview.get("operation_id").is_none());
        assert_eq!((rig.read)("notes/old.json").unwrap(), note(1, "old"));

        let revision = preview["revision"].clone();
        let upgraded = rig
            .call(
                upgrade,
                json!({ "upgrade": UPGRADE, "expected_revision": revision }),
            )
            .await
            .unwrap();
        assert_eq!(upgraded["revision"], revision);
        assert_eq!(upgraded["changed"], json!(["notes/old.json"]));
        assert!(upgraded["operation_id"]
            .as_str()
            .unwrap()
            .starts_with("format-2-"));
        assert_eq!(
            (rig.read)("notes/old.json").unwrap(),
            to_format_2("", "", &note(1, "old")).unwrap()
        );
        assert_eq!(
            (rig.read)("notes/current.json").unwrap(),
            note(2, "current")
        );
        assert_eq!((rig.read)("notes/broken.json").unwrap(), b"not json");

        let again = rig
            .call(upgrade, json!({ "upgrade": UPGRADE }))
            .await
            .unwrap();
        assert_eq!(again["changed"], json!([]));
        assert!(again.get("operation_id").is_none());
    }
}

#[tokio::test]
async fn an_upgrade_runs_again_over_what_it_upgraded_before() {
    for rig in rigs() {
        rig.seed();
        let upgrade = "walthamstow.workspace.upgrade";
        let first = rig
            .call(upgrade, json!({ "upgrade": UPGRADE }))
            .await
            .unwrap();
        (rig.write)("notes/old.json", &note(1, "old"));
        let second = rig
            .call(upgrade, json!({ "upgrade": UPGRADE }))
            .await
            .unwrap();
        assert_eq!(second["operation_id"], first["operation_id"]);
        assert_eq!(second["changed"], json!(["notes/old.json"]));
    }
}

#[tokio::test]
async fn an_upgrade_is_refused_when_its_documents_changed_after_the_preview() {
    for rig in rigs() {
        rig.seed();
        let upgrade = "walthamstow.workspace.upgrade";
        let preview = rig
            .preview(upgrade, json!({ "upgrade": UPGRADE }))
            .await
            .unwrap();
        (rig.write)("notes/old.json", &note(1, "edited"));
        let refused = rig
            .call(
                upgrade,
                json!({ "upgrade": UPGRADE, "expected_revision": preview["revision"] }),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, codes::UPGRADE_STALE);
        assert_eq!(refused.category, Category::Conflict);
        assert!(refused.retryable);
        assert_ne!(refused.details.unwrap()["revision"], preview["revision"]);
        assert_eq!((rig.read)("notes/old.json").unwrap(), note(1, "edited"));
    }
}

#[tokio::test]
async fn a_document_newer_than_the_engine_knows_refuses_every_upgrade() {
    for rig in rigs() {
        rig.seed();
        (rig.write)("notes/newer.json", &note(3, "newer"));
        for preview in [true, false] {
            let refused = rig
                .call_as(
                    operator(),
                    "walthamstow.workspace.upgrade",
                    json!({ "upgrade": UPGRADE }),
                    preview,
                )
                .await
                .unwrap_err();
            assert_eq!(refused.code, codes::DOCUMENT_NEWER);
            assert_eq!(refused.category, Category::Conflict);
            assert_eq!(
                refused.details.unwrap()["documents"],
                json!([{ "path": "notes/newer.json", "format": "3" }])
            );
        }
        assert_eq!((rig.read)("notes/old.json").unwrap(), note(1, "old"));
    }
}

#[tokio::test]
async fn an_unregistered_upgrade_is_not_found() {
    for rig in rigs() {
        let refused = rig
            .call(
                "walthamstow.workspace.upgrade",
                json!({ "upgrade": "format-9" }),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, codes::UPGRADE_UNKNOWN);
        assert_eq!(refused.category, Category::NotFound);
    }
}

#[tokio::test]
async fn every_operation_is_refused_while_another_engine_holds_the_workspace() {
    let calls = [
        ("walthamstow.workspace.status", json!({})),
        ("walthamstow.workspace.documents", json!({})),
        (
            "walthamstow.workspace.upgrade",
            json!({ "upgrade": UPGRADE }),
        ),
        ("walthamstow.workspace.recover", json!({})),
        ("walthamstow.journals.list", json!({})),
    ];
    for rig in rigs() {
        rig.seed();
        let held = (rig.hold_elsewhere)();
        for (name, input) in &calls {
            let refused = rig.call(name, input.clone()).await.unwrap_err();
            assert_eq!(refused.code, codes::WORKSPACE_HELD, "{name}");
            assert_eq!(refused.category, Category::Unavailable, "{name}");
            assert_eq!(refused.category.exit_code(), 69, "{name}");
            assert!(refused.retryable, "{name}");
        }
        drop(held);
        for (name, input) in calls.iter().cloned() {
            rig.call(name, input).await.unwrap();
        }
    }
}

#[tokio::test]
async fn a_daemon_runs_operations_under_the_lease_it_holds() {
    let lock = MemoryLock::new();
    let workspace = Arc::new(MemoryWorkspace::new("memory").directory("note", "notes"));
    let store: Arc<dyn LocalStore> = Arc::new(MemoryLocalStore::new());
    let daemon = compose(
        Lease::daemon(lock.acquire().unwrap()),
        workspace.clone(),
        store.clone(),
    );
    let outcome = daemon
        .call(
            &daemon.all(),
            operator(),
            "walthamstow.workspace.status",
            json!({}),
            RecordSink::default(),
            Cancellation::new(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(outcome.result.unwrap()["holder"], "daemon");
    let in_process = compose(Lease::in_process_memory(lock), workspace, store);
    let refused = in_process
        .call(
            &in_process.all(),
            operator(),
            "walthamstow.workspace.status",
            json!({}),
            RecordSink::default(),
            Cancellation::new(),
            false,
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, codes::WORKSPACE_HELD);
}

#[tokio::test]
async fn upgrades_and_recovery_need_the_admin_scope() {
    for rig in rigs() {
        rig.seed();
        let reader = grant(&[WORKSPACE_READ, JOURNALS_READ]);
        for (name, input) in [
            (
                "walthamstow.workspace.upgrade",
                json!({ "upgrade": UPGRADE }),
            ),
            ("walthamstow.workspace.recover", json!({})),
        ] {
            let refused = rig
                .call_as(reader.clone(), name, input, false)
                .await
                .unwrap_err();
            assert_eq!(refused.code, "insufficient_scope", "{name}");
        }
        rig.call_as(reader, "walthamstow.workspace.status", json!({}), false)
            .await
            .unwrap();
        assert_eq!((rig.read)("notes/old.json").unwrap(), note(1, "old"));
    }
}

#[tokio::test]
async fn journals_list_names_each_record_of_each_listed_journal() {
    for rig in rigs() {
        let key = JournalKey::new(PRODUCT_JOURNAL, ["session", "head"]).unwrap();
        rig.store
            .compare_and_swap(&key, None, b"held")
            .unwrap()
            .unwrap();
        let journals = rig
            .call("walthamstow.journals.list", json!({}))
            .await
            .unwrap();
        assert_eq!(
            journals,
            json!({ "journals": [
                { "journal": "collaboration-publications", "records": [] },
                { "journal": "product-records", "records": [["session", "head"]] },
            ]})
        );
    }
}

#[tokio::test]
async fn every_operation_declares_walthamstow_s_refusals_and_runs_against_the_workspace() {
    let rig = memory();
    let ours = rig
        .set
        .operations()
        .filter(|operation| operation.name.as_str().starts_with("walthamstow."))
        .collect::<Vec<_>>();
    assert_eq!(ours.len(), 5);
    let declared = codes::ALL
        .iter()
        .map(|code| (code.to_string(), codes::category(code).unwrap()))
        .collect::<BTreeMap<_, _>>();
    for operation in ours {
        assert_eq!(operation.refusals, declared, "{}", operation.name.as_str());
        assert_eq!(operation.locality, clapham::Locality::Workspace);
    }
}

#[tokio::test]
async fn recovery_finishes_an_upgrade_a_stop_interrupted() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let [notes] =
        walthamstow_workspace::paths::resolve_directories(root.path(), [("notes", "notes")])
            .unwrap();
    write_synced(DURABILITY, &notes.join("old.json"), &note(1, "old")).unwrap();
    let work = walthamstow_upgrades::workspace::work_dir(data.path(), "interrupted").unwrap();
    let path = Path::new("notes/old.json").to_path_buf();
    let upgraded = to_format_2("", "", &note(1, "old")).unwrap();
    write_synced(
        DURABILITY,
        &walthamstow_upgrades::workspace::prepared_dir(&work).join(&path),
        &upgraded,
    )
    .unwrap();
    walthamstow_upgrades::workspace::authorise(
        DURABILITY,
        &work,
        "interrupted",
        &BTreeMap::from([(path.clone(), note(1, "old"))]),
        &BTreeMap::from([(path, upgraded.clone())]),
    )
    .unwrap();
    let workspace =
        Arc::new(FileWorkspace::new(root.path(), data.path(), DURABILITY).directory("note", notes));
    let store: Arc<dyn LocalStore> = FileLocalStore::open(data.path(), DURABILITY).unwrap();
    let set = compose(Lease::in_process(root.path()), workspace, store);
    let all = set.all();
    let call = |name: &'static str| {
        set.call(
            &all,
            operator(),
            name,
            json!({}),
            RecordSink::default(),
            Cancellation::new(),
            false,
        )
    };
    let recovered = call("walthamstow.workspace.recover").await.unwrap();
    assert_eq!(
        recovered.result.unwrap(),
        json!({ "finished": ["interrupted"], "discarded": [] })
    );
    assert_eq!(
        std::fs::read(root.path().join("notes/old.json")).unwrap(),
        upgraded
    );
    let again = call("walthamstow.workspace.recover").await.unwrap();
    assert_eq!(
        again.result.unwrap(),
        json!({ "finished": [], "discarded": [] })
    );
}

#[test]
fn a_file_publication_refused_by_its_fence_leaves_no_journal_to_recover() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_synced(
        DURABILITY,
        &root.path().join("notes/old.json"),
        &note(1, "edited"),
    )
    .unwrap();
    let workspace = FileWorkspace::new(root.path(), data.path(), DURABILITY);
    let path = "notes/old.json".to_string();
    let refused = workspace
        .publish(
            "format-2-0123456789abcdef",
            &BTreeMap::from([(path.clone(), note(1, "old"))]),
            &BTreeMap::from([(path, note(2, "old"))]),
        )
        .unwrap_err();
    let refused = refused
        .downcast_ref::<walthamstow_kernel::KernelError>()
        .unwrap();
    assert_eq!(refused.reason_code(), Some(codes::UPGRADE_STALE));
    assert_eq!(
        std::fs::read(root.path().join("notes/old.json")).unwrap(),
        note(1, "edited")
    );
    assert_eq!(workspace.recover().unwrap(), Default::default());
}

/// Publishes an upgrade of two documents after one of them was written with its target bytes,
/// and checks that neither is replaced.
fn refuses_a_document_already_at_its_target(
    workspace: &dyn Workspace,
    write: &dyn Fn(&str, &[u8]),
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) {
    let (first, second) = (
        "notes/first.json".to_string(),
        "notes/second.json".to_string(),
    );
    write(&first, &note(1, "first"));
    write(&second, &note(2, "second"));
    let refused = workspace
        .publish(
            "format-2-0123456789abcdef",
            &BTreeMap::from([
                (first.clone(), note(1, "first")),
                (second.clone(), note(1, "second")),
            ]),
            &BTreeMap::from([
                (first.clone(), note(2, "first")),
                (second.clone(), note(2, "second")),
            ]),
        )
        .unwrap_err();
    let refused = refused
        .downcast_ref::<walthamstow_kernel::KernelError>()
        .unwrap();
    assert_eq!(refused.reason_code(), Some(codes::UPGRADE_STALE));
    assert_eq!(refused.data.as_ref().unwrap()["path"], second);
    assert_eq!(read(&first).unwrap(), note(1, "first"));
    assert_eq!(workspace.recover().unwrap(), Default::default());
}

#[test]
fn a_publication_is_refused_when_a_document_already_holds_its_target() {
    let memory = MemoryWorkspace::new("memory");
    refuses_a_document_already_at_its_target(
        &memory,
        &|path, bytes| memory.write(path, bytes),
        &|path| memory.read(path),
    );
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    refuses_a_document_already_at_its_target(
        &FileWorkspace::new(root.path(), data.path(), DURABILITY),
        &|path, bytes| write_synced(DURABILITY, &root.path().join(path), bytes).unwrap(),
        &|path| std::fs::read(root.path().join(path)).ok(),
    );
}

// Windows separates components with a backslash, so no name holds one.
#[cfg(unix)]
#[tokio::test]
async fn a_document_named_with_a_backslash_is_refused_rather_than_named_as_another() {
    let rig = files();
    rig.seed();
    (rig.write)("notes/a\\b.json", &note(1, "backslash"));
    for name in [
        "walthamstow.workspace.status",
        "walthamstow.workspace.documents",
    ] {
        let refused = rig.call(name, json!({})).await.unwrap_err();
        assert_eq!(refused.code, codes::WORKSPACE_FAILURE, "{name}");
        assert!(refused.message.contains("backslash"), "{}", refused.message);
    }
    let refused = rig
        .call(
            "walthamstow.workspace.upgrade",
            json!({ "upgrade": UPGRADE }),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, codes::WORKSPACE_FAILURE);
    assert_eq!((rig.read)("notes/a\\b.json").unwrap(), note(1, "backslash"));
    assert!((rig.read)("notes/a/b.json").is_none());
}

#[test]
fn an_upgrade_id_outside_the_alphabet_or_registered_twice_is_refused() {
    let ops = || {
        WalthamstowOps::new(
            Lease::in_process_memory(MemoryLock::new()),
            Arc::new(MemoryWorkspace::new("memory")),
            Arc::new(MemoryLocalStore::new()),
        )
    };
    for id in ["", "Format-2", "format/2", "format 2"] {
        assert!(ops().upgrade(id, to_format_2).is_err(), "{id:?}");
    }
    assert!(ops()
        .upgrade(UPGRADE, to_format_2)
        .unwrap()
        .upgrade(UPGRADE, to_format_2)
        .is_err());
}
