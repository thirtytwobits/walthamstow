//! Copyright (c) Scott A Dixon
//!
//! The engine kernel's workspace mechanics, whatever entities a product keeps there: each
//! collaborative entity's documents, the canonical store they are published to and the outbox
//! of those it has not yet acknowledged, the directories documents live in, finding and reading
//! documents, loading and applying their schemas, mount paths, and the walk of a document schema
//! into field guidance.

pub mod canonical;
pub mod collaboration;
pub mod documents;
pub mod mounts;
pub mod outbox;
pub mod paths;
pub mod reason_codes;
pub mod schema_describe;
pub mod schemas;
