//! Copyright (c) Scott A Dixon
//!
//! The engine kernel's workspace mechanics, whatever entities a product keeps there: each
//! collaborative entity's documents, the directories documents live in, finding and reading
//! documents, loading and applying their schemas, mount paths, the walk of a document schema
//! into field guidance, and the journal of accepted documents still waiting for their files.

pub mod collaboration;
pub mod documents;
pub mod mounts;
pub mod paths;
pub mod publications;
pub mod reason_codes;
pub mod schema_describe;
pub mod schemas;
