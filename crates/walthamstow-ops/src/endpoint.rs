//! Copyright (c) Scott A Dixon
//!
//! The daemon's endpoint record: where its MCP server listens, and the local credential a CLI
//! on the same machine presents to it. The daemon publishes the record in its local store at
//! start and retires it at stop. A CLI that names no endpoint finds the daemon through it.
//!
//! [`FileLocalStore`](walthamstow_kernel::local_store::FileLocalStore) keeps the record as a
//! file under `journals/daemon/`, readable by its owner alone on Unix. An engine with no disk
//! is configured with its endpoint, not discovered.

use serde::{Deserialize, Serialize};
use std::fmt;
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::local_store::{JournalKey, LocalStore, Tag};
use walthamstow_kernel::{KernelError, KernelResult};

/// The journal the endpoint record is kept in.
pub const JOURNAL: Journal = Journal::new("daemon");

/// The record's key within [`JOURNAL`].
const RECORD: &str = "endpoint";

/// Where the daemon's MCP server listens, and the credential a local CLI presents to it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointRecord {
    /// The MCP endpoint's URL.
    pub endpoint: String,
    /// The bearer token the daemon accepts from a local CLI.
    pub token: String,
}

impl fmt::Debug for EndpointRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EndpointRecord")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

fn key() -> JournalKey {
    JournalKey::new(JOURNAL, [RECORD]).expect("the record's name is not empty")
}

/// The record as the store keeps it: with the id of the publication that wrote it, so no two
/// publications store the same bytes, and no publication retires another's record.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    endpoint: String,
    token: String,
    publication: String,
}

/// The endpoint record a daemon published, until it retires it.
#[derive(Debug)]
#[must_use = "a published record stays until it is retired"]
pub struct Published {
    tag: Tag,
}

impl EndpointRecord {
    /// Publishes the record in `store`, over any a daemon that stopped without retiring its
    /// own left there. The daemon holds the workspace, so no other daemon's record is live.
    /// Each publication is stored under an id of its own, so retiring it never removes a
    /// record a later publication wrote, the same record included.
    pub fn publish(&self, store: &dyn LocalStore) -> KernelResult<Published> {
        let stored = Stored {
            endpoint: self.endpoint.clone(),
            token: self.token.clone(),
            publication: uuid::Uuid::new_v4().to_string(),
        };
        let bytes = serde_json::to_vec(&stored).expect("an endpoint record encodes");
        let left = store.get(&key())?.map(|(_, tag)| tag);
        let tag = store
            .compare_and_swap(&key(), left.as_ref(), &bytes)?
            .ok_or_else(|| {
                KernelError::conflict("The endpoint record changed while it was published.")
            })?;
        Ok(Published { tag })
    }

    /// The record `store` holds, if a daemon published one.
    pub fn discover(store: &dyn LocalStore) -> KernelResult<Option<Self>> {
        store
            .get(&key())?
            .map(|(bytes, _)| {
                let stored: Stored = serde_json::from_slice(&bytes).map_err(|error| {
                    KernelError::internal(format!("The endpoint record cannot be read: {error}"))
                })?;
                Ok(Self {
                    endpoint: stored.endpoint,
                    token: stored.token,
                })
            })
            .transpose()
    }

    /// Where a CLI finds the daemon: `explicit`, an endpoint a flag or the environment names,
    /// or else the record `store` holds; `None` when neither names one.
    pub fn resolve(explicit: Option<Self>, store: &dyn LocalStore) -> KernelResult<Option<Self>> {
        match explicit {
            Some(explicit) => Ok(Some(explicit)),
            None => Self::discover(store),
        }
    }
}

impl Published {
    /// Removes the record from `store` at stop, and returns whether it did. A record another
    /// daemon has published since is left in place.
    pub fn retire(self, store: &dyn LocalStore) -> KernelResult<bool> {
        store.remove(&key(), &self.tag)
    }
}
