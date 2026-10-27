//! Copyright (c) Scott A Dixon
//!
//! What the product tells Walthamstow about its documents' formats: the format each document
//! is in, and the upgrades that bring a document to the format the product reads.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The format a document is in, as the product reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum FormatStatus {
    /// The format the product reads.
    Current,
    /// An earlier format, which the registered upgrade `upgrade` brings to the current one.
    Upgradable { upgrade: String },
    /// A format newer than the product knows, named as the document names it.
    Newer { format: String },
    /// The product cannot tell the document's format.
    Unreadable { message: String },
}

/// Tells the format of one entity's documents.
pub trait DocumentFormat: Send + Sync {
    /// The format of the document at `path`, relative to the workspace root, holding `bytes`.
    fn status(&self, path: &str, bytes: &[u8]) -> FormatStatus;
}

impl<F> DocumentFormat for F
where
    F: Fn(&str, &[u8]) -> FormatStatus + Send + Sync,
{
    fn status(&self, path: &str, bytes: &[u8]) -> FormatStatus {
        self(path, bytes)
    }
}

/// Upgrades a document from an earlier format.
pub trait Upgrade: Send + Sync {
    /// The document of `entity` at `path`, holding `bytes`, in the format this upgrade
    /// produces; or why it cannot be upgraded.
    fn apply(&self, entity: &str, path: &str, bytes: &[u8]) -> Result<Vec<u8>, String>;
}

impl<F> Upgrade for F
where
    F: Fn(&str, &str, &[u8]) -> Result<Vec<u8>, String> + Send + Sync,
{
    fn apply(&self, entity: &str, path: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self(entity, path, bytes)
    }
}
