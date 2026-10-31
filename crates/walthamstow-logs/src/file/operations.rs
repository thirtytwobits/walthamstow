//! Copyright (c) Scott A Dixon
//!
//! The writes that change what a document has committed, `create`, `rewind`, `replace` and
//! `remove`, publish through the kernel's fenced publication. Each stages the files it writes in
//! the engine's data directory, under `staging/log-operations/<id>/`, and a record in the
//! `log-operations` journal authorises publishing them over the document's files, each only
//! while it holds what was read. A removal also removes the document's directory whole.
//!
//! Recovery finishes what a record authorises and discards staging no record authorises. A
//! publication whose file changed after it was authorised is discarded, since the files are the
//! record.

use crate::layout::DocumentLayout;
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use walthamstow_kernel::durability::{create_dir_all_in, write_synced_in, WriteDurability};
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::journals::{Journal, Staging};
use walthamstow_kernel::local_store::{JournalKey, LocalStore};
use walthamstow_kernel::publication::{publish, publish_first, Replacement};
use walthamstow_kernel::reason_codes::PUBLICATION_STALE;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// The journal whose records authorise publications.
pub(super) const JOURNAL: Journal = Journal::new("log-operations");

/// Where a publication's files are staged.
pub(super) const STAGING: Staging = Staging::new("log-operations");

/// The format of an operation record this release writes and reads.
const FORMAT: u32 = 1;

/// One file an operation writes or removes: its name in the document's directory, the hash it
/// held when read or none when there was no file, and its new bytes or none to remove it.
pub(super) struct Change {
    pub name: String,
    pub source: Option<String>,
    pub target: Option<Vec<u8>>,
}

/// An operation's record.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    format: u32,
    entity: String,
    resource: String,
    replacements: Vec<Replacement>,
    remove_directory: bool,
}

/// How a publication ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Published {
    /// Every file holds its target, and a removal's directory is gone.
    Done,
    /// A file holds neither what was read nor its target, so nothing was written.
    Stale,
}

/// What recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogRecovery {
    /// The documents whose publication a record authorised, which recovery finished.
    pub finished: Vec<DocumentId>,
    /// The documents whose publication a record authorised and whose files changed after, whose
    /// publication recovery discarded.
    pub stale: Vec<DocumentId>,
    /// The operations whose staging no record authorised, which recovery discarded.
    pub discarded: Vec<String>,
}

/// The journal and staging of the operations of one store.
#[derive(Debug)]
pub(super) struct Operations {
    local: Arc<dyn LocalStore>,
    /// The engine's data directory, through which the staging is reached.
    data: Dir,
    durability: WriteDurability,
}

fn internal(id: &str, error: impl std::fmt::Display) -> KernelError {
    KernelError::internal(format!("Log operation {id}: {error}"))
}

impl Operations {
    /// The operations kept in `local`, staged under the data directory at `data_dir`.
    pub fn new(
        local: Arc<dyn LocalStore>,
        data_dir: &Path,
        durability: WriteDurability,
    ) -> KernelResult<Self> {
        let data = walthamstow_kernel::files::open_dir(data_dir)
            .map_err(|error| internal("the data directory", error))?;
        Ok(Self {
            local,
            data,
            durability,
        })
    }

    /// The staging of every operation, or `None` when there is none.
    fn staging(&self) -> KernelResult<Option<Dir>> {
        STAGING
            .dir(&self.data)
            .map_err(|error| internal("staging", error))
    }

    /// The staging of every operation, created when absent.
    fn staging_root(&self) -> KernelResult<Dir> {
        STAGING
            .create_dir(self.durability, &self.data)
            .map_err(|error| internal("staging", error))
    }

    /// The staging of `id` within `root`, created when absent. Anything there but a directory, a
    /// link included, is refused unfollowed.
    fn operation_dir(&self, root: &Dir, id: &str) -> KernelResult<Dir> {
        match root.symlink_metadata(id) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(internal(
                    id,
                    "its staging is not a directory; it is not followed or opened",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_dir_all_in(self.durability, root, Path::new(id))
                    .map_err(|error| internal(id, error))?;
            }
            Err(error) => return Err(internal(id, error)),
        }
        root.open_dir(id).map_err(|error| internal(id, error))
    }

    fn key(id: &str) -> KernelResult<JournalKey> {
        JournalKey::new(JOURNAL, [id])
    }

    /// The record authorising `id`.
    fn record(&self, id: &str) -> KernelResult<Record> {
        let (bytes, _) = self
            .local
            .get(&Self::key(id)?)?
            .ok_or_else(|| internal(id, "no record authorises it"))?;
        let record: Record = serde_json::from_slice(&bytes).map_err(|error| internal(id, error))?;
        if record.format != FORMAT {
            return Err(internal(
                id,
                format!(
                    "its record is in format {}, which another release wrote; that release \
                     finishes it",
                    record.format
                ),
            ));
        }
        Ok(record)
    }

    /// Stages `changes` to `document` and authorises publishing them, then removing the
    /// document's directory when `remove_directory`. Returns the operation's id.
    pub fn authorise(
        &self,
        document: &DocumentId,
        changes: &[Change],
        remove_directory: bool,
    ) -> KernelResult<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let work = self.operation_dir(&self.staging_root()?, &id)?;
        let mut replacements = Vec::with_capacity(changes.len());
        for change in changes {
            if let Some(bytes) = &change.target {
                write_synced_in(self.durability, &work, Path::new(&change.name), bytes)
                    .map_err(|error| internal(&id, error))?;
            }
            replacements.push(Replacement {
                path: PathBuf::from(&change.name),
                source_hash: change.source.clone(),
                target_hash: change.target.as_deref().map(sha256_hex),
            });
        }
        let record = Record {
            format: FORMAT,
            entity: document.entity().to_owned(),
            resource: document.resource().to_owned(),
            replacements,
            remove_directory,
        };
        let bytes = serde_json::to_vec(&record).expect("an operation record serialises");
        self.local
            .compare_and_swap(&Self::key(&id)?, None, &bytes)?
            .ok_or_else(|| internal(&id, "it is authorised already"))?;
        Ok(id)
    }

    /// Publishes what the record of `id` authorises over its document's files: as a first
    /// publication when `first`, which refuses a file already at its target, and otherwise
    /// accepting one, as finishing an interrupted publication does.
    pub fn publish(
        &self,
        layout: &DocumentLayout,
        id: &str,
        first: bool,
    ) -> KernelResult<(DocumentId, Published)> {
        let record = self.record(id)?;
        let document = DocumentId::new(&record.entity, &record.resource);
        let Some(live) = layout.document_dir(&document)? else {
            // A removal that removed the directory may not have made the removal durable; any
            // other publication's directory was removed after it was authorised.
            if !record.remove_directory {
                return Ok((document, Published::Stale));
            }
            layout.remove_document_dir(self.durability, &document)?;
            return Ok((document, Published::Done));
        };
        let staged = self.operation_dir(&self.staging_root()?, id)?;
        let published = if first {
            publish_first(self.durability, &live.dir, &staged, &record.replacements)
        } else {
            publish(self.durability, &live.dir, &staged, &record.replacements)
        };
        match published {
            Ok(()) => {}
            Err(error)
                if error
                    .downcast_ref::<KernelError>()
                    .and_then(KernelError::reason_code)
                    == Some(PUBLICATION_STALE) =>
            {
                return Ok((document, Published::Stale))
            }
            Err(error) => return Err(KernelError::from(error)),
        }
        if record.remove_directory {
            drop(live);
            layout.remove_document_dir(self.durability, &document)?;
        }
        Ok((document, Published::Done))
    }

    /// Removes the record authorising `id`, then its staging, so an interruption between the two
    /// leaves staging no record authorises.
    pub fn retire(&self, id: &str) -> KernelResult<()> {
        let key = Self::key(id)?;
        if let Some((_, tag)) = self.local.get(&key)? {
            self.local.remove(&key, &tag)?;
        }
        if let Some(staging) = self.staging()? {
            remove_staging(&staging, id).map_err(|error| internal(id, error))?;
        }
        Ok(())
    }

    /// Finishes each publication a record authorises, discards each whose file changed after
    /// it was authorised, and discards staging no record authorises.
    pub fn recover(&self, layout: &DocumentLayout) -> KernelResult<LogRecovery> {
        let mut recovery = LogRecovery::default();
        let mut authorised = std::collections::BTreeSet::new();
        for key in self.local.list(&JournalKey::journal(JOURNAL))? {
            let [id] = key.names() else {
                return Err(internal(
                    &key.to_string(),
                    "it is not an operation's record",
                ));
            };
            authorised.insert(id.clone());
            let (document, published) = self.publish(layout, id, false)?;
            self.retire(id)?;
            match published {
                Published::Done => recovery.finished.push(document),
                Published::Stale => recovery.stale.push(document),
            }
        }
        let Some(staging) = self.staging()? else {
            return Ok(recovery);
        };
        let entries = staging
            .entries()
            .map_err(|error| internal("staging", error))?;
        let mut discarded = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| internal("staging", error))?;
            let name = entry.file_name();
            let Some(id) = name.to_str() else {
                return Err(internal(
                    "staging",
                    format!("{} is not named in UTF-8", name.to_string_lossy()),
                ));
            };
            if authorised.contains(id) {
                continue;
            }
            if !entry
                .file_type()
                .map_err(|error| internal(id, error))?
                .is_dir()
            {
                return Err(internal(
                    id,
                    "its staging is not a directory; recovery does not follow or open it",
                ));
            }
            remove_staging(&staging, id).map_err(|error| internal(id, error))?;
            discarded.push(id.to_owned());
        }
        discarded.sort();
        recovery.discarded = discarded;
        Ok(recovery)
    }
}

/// Removes the staging of `id` from `staging`. One already gone is left so.
fn remove_staging(staging: &Dir, id: &str) -> std::io::Result<()> {
    match staging.remove_dir_all(id) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
