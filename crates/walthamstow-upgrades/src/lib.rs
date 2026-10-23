//! Copyright (c) Scott A Dixon
//!
//! The engine kernel's upgrade framework: upgrades of a workspace's documents, journalled in the
//! engine's local store and published to the canonical store, each document fenced by what the
//! upgrade read; and staged files published over live ones, each fenced by the hash it held
//! when staged.

pub mod publication;
pub mod workspace;
