//! Copyright (c) Scott A Dixon
//!
//! Walthamstow's operations over a seeded workspace in memory, as a product composes them:
//! one entity of notes in two formats, the upgrade between them, and a journal of its own.
//! Every call to [`contributions`] seeds the same workspace, so each front end Clapham's
//! conformance harness drives starts from one state.

use clapham::{
    Bounds, ComposeOptions, ComposedSet, CompositionError, Contribution, ExecutionMode,
    OAuthContext, OAuthResource,
};
use serde_json::{json, Value};
use std::sync::Arc;
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::local_store::{JournalKey, LocalStore, MemoryLocalStore};
use walthamstow_ops::operations::{JOURNALS_READ, WORKSPACE_ADMIN, WORKSPACE_READ};
use walthamstow_ops::{Entity, FormatStatus, Lease, MemoryLock, MemoryWorkspace, WalthamstowOps};

/// The issuer of the grants the example's callers hold.
pub const ISSUER: &str = "https://issuer.test";
/// The audience the example's daemon serves.
pub const AUDIENCE: &str = "https://walthamstow.test/mcp";
/// The upgrade from a note's first format to its second.
pub const UPGRADE: &str = "format-2";
/// The product's own journal.
pub const PRODUCT_JOURNAL: Journal = Journal::new("product-records");

/// An operator's grant, holding every Walthamstow scope.
pub fn operator() -> OAuthContext {
    OAuthContext::from_verified(
        ISSUER,
        AUDIENCE,
        "operator",
        [WORKSPACE_READ, WORKSPACE_ADMIN, JOURNALS_READ],
    )
}

/// A note's format, named by its `format` field: 2 is current and 1 upgrades to it.
pub fn note_format(_path: &str, bytes: &[u8]) -> FormatStatus {
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

/// A note in its second format.
pub fn to_format_2(_entity: &str, _path: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut note: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    note["format"] = json!(2);
    Ok(serde_json::to_vec(&note).expect("a note encodes"))
}

/// A note in `format` saying `text`.
pub fn note(format: u64, text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "format": format, "text": text })).expect("a note encodes")
}

/// The seeded workspace, its local store and their operations, run under `lease`.
pub fn operations(lease: Lease) -> WalthamstowOps {
    let workspace = MemoryWorkspace::new("memory").directory("note", "notes");
    workspace.write("notes/old.json", note(1, "old"));
    workspace.write("notes/current.json", note(2, "current"));
    workspace.write("notes/broken.json", b"not json".to_vec());
    let store = MemoryLocalStore::new();
    let key = JournalKey::new(PRODUCT_JOURNAL, ["session", "head"]).expect("a named record");
    store
        .compare_and_swap(&key, None, b"held")
        .expect("a record in memory")
        .expect("an empty store");
    WalthamstowOps::new(lease, Arc::new(workspace), Arc::new(store))
        .entity(Entity::new("note", "notes", note_format))
        .upgrade(UPGRADE, to_format_2)
        .expect("a valid upgrade id")
        .journal(PRODUCT_JOURNAL)
}

/// A freshly seeded workspace's `walthamstow` contribution, run in-process under a lock in
/// memory.
pub fn contributions() -> Vec<Contribution> {
    vec![operations(Lease::in_process_memory(MemoryLock::new())).contribution()]
}

/// A freshly seeded workspace's operations, composed as its daemon serves them.
pub fn composed() -> Result<ComposedSet, CompositionError> {
    clapham::compose_with_options(
        contributions(),
        Bounds::default(),
        ComposeOptions {
            execution: ExecutionMode::Daemon,
            oauth: Some(OAuthResource {
                issuer: ISSUER.into(),
                audience: AUDIENCE.into(),
            }),
            ..ComposeOptions::default()
        },
    )
}
