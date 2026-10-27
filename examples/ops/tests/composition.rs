//! Copyright (c) Scott A Dixon
//!
//! Walthamstow's and Clerkenwell's operations in one composed set: both pieces take one
//! Clapham, so a process holding the workspace composes Walthamstow's operations with
//! Clerkenwell's recovery operations.

use clapham::{Bounds, Cancellation, OAuthContext, RecordSink};
use clerkenwell_events::{ActorKind, Principal};
use clerkenwell_ops::{Invalid, OperationsHost};
use clerkenwell_session::AuthoringStates;
use clerkenwell_store::{CollaborationStores, CommitPolicy, PeerKey};
use ops_example::operator;
use serde_json::{json, Value};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use walthamstow_ops::{Lease, MemoryLock};

/// The operator, as Clerkenwell's principal.
struct Host;

impl OperationsHost for Host {
    fn principal(&self, auth: &OAuthContext) -> Option<Principal> {
        Some(Principal::new(auth.subject(), ActorKind::Human))
    }

    fn validate(&self, _entity: &str, _document: &Value) -> Result<(), Invalid> {
        Ok(())
    }
}

fn authoring() -> AuthoringStates {
    let stores = CollaborationStores::new(
        &[],
        CommitPolicy {
            retained_operations: 8,
            attempts: NonZeroU32::MIN,
            backoff: Duration::ZERO,
        },
        PeerKey::new([7; 32]).unwrap(),
    );
    AuthoringStates::new(stores, &[], |_, _| None)
}

#[tokio::test]
async fn a_process_holding_the_workspace_composes_both_pieces_operations() {
    let set = clapham::compose(
        [
            ops_example::operations(Lease::in_process_memory(MemoryLock::new())).contribution(),
            clerkenwell_ops::contribute_held(authoring(), Arc::new(Host)),
        ],
        Bounds::default(),
    )
    .unwrap();
    let namespaces: Vec<_> = set
        .operations()
        .filter_map(|operation| operation.name.namespace())
        .collect();
    assert!(namespaces.contains(&"walthamstow"));
    assert!(namespaces.contains(&"clerkenwell"));

    let outcome = set
        .call(
            &set.all(),
            operator(),
            "walthamstow.workspace.status",
            json!({}),
            RecordSink::default(),
            Cancellation::new(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(outcome.result.unwrap()["holder"], "process");
}
