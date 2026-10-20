//! Engine operation journals.
//!
//! A journal records how far an operation got, so a retry or a restart can
//! finish it. Journals belong to the engine, not to the workspace: they live
//! under the engine data directory beside the database. Durable workspace
//! content goes in a configured workspace path.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub enum Journal {
    CharacterLifecycle,
    FlowExecutions,
    FlowCheckpoints,
    RuntimeAccounting,
    CollaborationPublications,
    SessionUpgrades,
    WorkspaceUpgrades,
}

impl Journal {
    fn directory(self) -> &'static str {
        match self {
            Journal::RuntimeAccounting => "runtime-accounting",
            Journal::FlowExecutions => "flow-executions",
            Journal::FlowCheckpoints => "flow-checkpoints",
            Journal::CharacterLifecycle => "character-lifecycle",
            Journal::CollaborationPublications => "collaboration-publications",
            Journal::SessionUpgrades => "session-upgrades",
            Journal::WorkspaceUpgrades => "workspace-upgrades",
        }
    }

    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join("journals").join(self.directory())
    }
}

/// Where every session's journals live under the engine data directory
/// `data_dir`, one directory per session that has any.
pub fn session_journals_root(data_dir: &Path) -> PathBuf {
    data_dir.join("journals").join("sessions")
}

/// Where a session's journals live under the engine data directory
/// `data_dir`: its submissions, history restorations, retained text commands,
/// and its text store's records. They share the session's lifetime.
pub fn session_journal_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    session_journals_root(data_dir).join(session_id)
}

/// Where an operation stages what it promotes into the sessions directory by
/// rename, and parks what it replaces. Staging belongs to the engine as a
/// journal does, so it lives under the engine data directory, which the store
/// requires to share a filesystem with the sessions directory. What it holds is
/// gone once the operation ends, or at the next start.
#[derive(Debug, Clone, Copy)]
pub enum Staging {
    SessionPublishing,
    SessionBackups,
    SessionImports,
    SessionUpgrades,
}

impl Staging {
    fn directory(self) -> &'static str {
        match self {
            Staging::SessionPublishing => "session-publishing",
            Staging::SessionBackups => "session-backups",
            Staging::SessionImports => "session-imports",
            Staging::SessionUpgrades => "session-upgrades",
        }
    }

    pub fn root(self, data_dir: &Path) -> PathBuf {
        data_dir.join("staging").join(self.directory())
    }
}
