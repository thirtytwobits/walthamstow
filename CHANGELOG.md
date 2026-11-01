# Changelog

## 0.8.0

### Breaking

- The crates need Rust 1.95 or later.
- Walthamstow builds on Clerkenwell 0.13.0 and Clapham 0.4.0. A host that also depends on either takes
  its tag; Cargo treats each Git tag as a separate crate.

## 0.7.0

### Breaking

- `leases::LeaseScope` is `DocumentId`, in the kernel's `document` module, and converts to and from
  Clerkenwell's `CollaborationDocumentId`. `Lease::scope` is `Lease::document`.
- `walthamstow_upgrades::publication` is the kernel's `publication` module, over `cap-std`, which
  the kernel now depends on. `publish` and `publish_first` take the live and staged roots as
  `cap_std::fs::Dir`, and resolve every file through them; `file_hash` and `read_regular` take a
  `Dir`. `Replacement::target_hash` is an `Option`, and `None` removes the live file. The kernel
  refuses with `publication_stale` and `publication_paths_collide`; `walthamstow-upgrades` names
  them `upgrade_stale` and `upgrade_paths_collide`, and `workspace::check_paths` checks an
  upgrade's paths.
- `journals::Journal` and `journals::Staging` resolve their directories through the data
  directory held as a `cap_std::fs::Dir`: `dir` opens one and `create_dir` creates it, and each
  refuses a link at `journals/`, `staging/` or the directory itself. `unlinked_root` is removed;
  `root` names where the directory lies. `files::identity` and `files::read_files` take a `Dir`.
  `FileLocalStore` reaches every journal and record through the data directory's handle, and takes
  the directory's identity from it.
- `walthamstow_upgrades::workspace` works through the workspace root and the data directory held
  as `cap_std::fs::Dir`: `stage`, `authorise`, `finish`, `finish_first`, `pending`, `recover`,
  `retire` and `abandon` take them, and an operation id in place of a work directory's path.
  `stage` writes an upgrade's prepared files, and `check_operation_id` checks an id; `work_dir`
  and `prepared_dir` are removed. `retire` returns whether the id is free to start an upgrade
  under. `walthamstow-ops`' `FileWorkspace::new` opens the workspace root and the data directory
  once, and returns a `Result`.
- `walthamstow-workspace` resolves through `cap_std::fs::Dir` handles. `paths::resolve_directories`
  takes the workspace root's handle and a durability, creates each configured directory that does
  not exist, and returns a `ConfiguredDirectory`, the directory held open with where it lies
  relative to the root, every link resolved; overlap is compared case-folded where it lies.
  `documents::list_documents` walks a `Dir` and returns paths relative to it, and refuses a link
  out of the directory. `mounts::open_mount` opens a mount, and `mounts::resolve_mount_path` takes
  its handle and returns the mount-relative path to open through it. `walthamstow-ops`'
  `FileWorkspace::directory` takes a `ConfiguredDirectory`.

### Added

- `durability::write_synced_in` writes a file durably within a `cap_std::fs::Dir`, and
  `create_dir_all_in` creates directories durably within one. `WriteDurability::sync_dir` syncs the
  directory a `Dir` holds. `files::open_dir` opens a directory as a `Dir`, sharing deletion on
  Windows so that it opens a workspace root an engine holds, and `files::open_dir_to_read` opens
  one without asking for write access.
- `walthamstow-logs` keeps log documents: a head, a format and an ordered log of records, written
  by one lease holder at a time. `LogStore` creates, commits to, rewinds, replaces and removes a
  document under its lease and over the revision its writer read, retries a write by its id under
  the lease that made it, and reads a document's head, a page of its records or a page of an
  entity's documents, reporting a broken document by a `LogFault`. A `Revision` is the document's
  incarnation and a tag the adapter chooses. `MemoryLogStore` keeps them in memory. The
  `conformance` feature adds the suite every adapter runs: each rule as a named test, and a
  property test against a reference model.
- `walthamstow-logs` keeps blobs: bytes a record or a head names, kept once per document under a
  `BlobKind` the product registers and named by a `BlobRef` of kind and SHA-256. `BlobStore` puts
  and removes blobs under the document's lease, and reads them checked against their digest,
  reporting an absent or corrupt blob by a `BlobFault`. `MemoryBlobStore` keeps them in memory.
  `FileBlobStore` keeps each as a file under a `DocumentLayout`, the document directory the file
  log store shares: a `cap-std` root no operation leaves, an entity directory per registered
  entity, and a resource directory that is the resource percent-encoded. The conformance suite
  has a blob section.
- `walthamstow-logs` keeps log documents as files people can read and edit. `FileLogStore` keeps
  each as a head file and a log file under a `DocumentLayout`, named for each entity by
  `LogFiles`. A line counts once the head's next position is past it: a commit appends its lines,
  then replaces the head. `create`, `rewind`, `replace` and `remove` publish through the kernel's
  publication under the `log-operations` journal, which opening the store recovers into a
  `LogRecovery`. A revision is a hash of the files, so a write over a revision an edit outside the
  store replaced is refused with `log_stale`. A stamp cache in the `log-stamps` journal spares
  the hash of a log file that has settled, and `verify` hashes every document of an entity.

## 0.6.0

### Breaking

- `FileLeases::open` takes the workspace lock, and `workspace_lock` is no longer public.
  `walthamstow-ops`' in-process lease opens `FileLeases`.

### Added

- `leases` grants a holder a document until it releases the lease or the lease lapses. `Leases`
  acquires, renews and releases leases, and `fence` runs a write only while its lease stands,
  holding the document against every other grant until the write returns, or refuses it with
  `lease_lost`. `fenced` does the same for a write that returns a value or its own error. Each grant carries an epoch later than every grant before it in the process.
  `FileLeases` hold the workspace lock and grant leases that never lapse. `MemoryLeases` grant
  leases that lapse a term after their grant or last renewal, on the clock they are given.

## 0.5.0

### Breaking

- Walthamstow builds on Clerkenwell 0.12.0 and Clapham 0.3.3, the Clapham that `clerkenwell-ops`
  takes. A host that also depends on either takes the same tags; Cargo treats each Git tag as a
  separate crate.

## 0.4.1

### Added

- The crates declare Rust 1.88 as their minimum supported version.

### Documentation

- `RELEASING.md` holds the release procedure the London Project shares among its pieces.

## 0.4.0

- Breaking: Walthamstow builds on Clerkenwell 0.11.0 and Clapham 0.3.2, the Clapham that
  `clerkenwell-ops` takes, so a product composes both pieces' operations in one set.
- `CollaborativeDocuments::import` answers a retry of an operation the document retains as a
  duplicate, with the result the operation was accepted with.
- `docs/contract.md` states that a commit made to the store other than through
  `CollaborativeDocuments`, such as through `clerkenwell-ops`, is not marked for publication.

## 0.3.0

- Breaking: Walthamstow builds on Clerkenwell 0.10.0.
  - `CollaborativeDocuments::published` and `AcceptedRead` carry a `CollaborationVersion` in
    place of a generation, so files rendering a removed document's history never clear the mark
    of a document created again under its name.
  - `authoring_state` takes what the peer holds as a `CollaborationHeld`, `bootstrap_update` takes
    the incarnation of the history it is given, and `remove` returns the version it removed.
  - The collaboration cache keeps envelope format 5. `prepare_cache_root` clears a cache in an
    earlier format, which the product rebuilds from its workspace files.
- `LocalStore` keeps the engine's local records: `get`, `compare_and_swap`, `list` and `remove`,
  each record under a `JournalKey` and replaced only over the `Tag` its writer read.
  `FileLocalStore` owns each journal's directory and keeps each record in a file named by a
  digest of its key, which the file holds, so a key never becomes a path. Every open of one data
  directory shares one store, which `files::identity` tells by the directory itself, not the
  path that reached it. On Unix, it writes each record, and creates each directory, for its
  owner alone. `MemoryLocalStore` keeps records in memory.
- Breaking: `PublicationJournal::open` takes the `LocalStore` its marks are kept in, and every
  open over one store shares one journal. A mark's record is named by its document. Marks an
  earlier release left are refused: that release publishes what they mark.
- Breaking: `workspace::recover` returns a `Recovery`: the upgrades it published, and those whose
  staging it discarded, by operation id. It refuses an upgrade's directory whose name is not
  UTF-8.
- `publication::publish` refuses a file changed after staging with the reason `upgrade_stale`.
  `publication::publish_first` and `workspace::finish_first` publish an upgrade for the first
  time, refusing a file already at its target too. `publication::check_publication_paths`
  refuses two paths that differ only in case with the reason `upgrade_paths_collide`, and
  `workspace::authorise` refuses them before writing. `workspace::pending` names each upgrade a
  journal authorises that is still to finish. `workspace::retire` removes a completed upgrade's
  directory, and `workspace::abandon` an upgrade whose first publication was refused, its journal
  first.
- `walthamstow-ops` contributes `walthamstow.workspace.status`, `.documents`, `.upgrade` and
  `.recover`, and `walthamstow.journals.list`, to Clapham 0.3.0. Each runs under a `Lease`: the
  daemon's, or one an in-process front end takes for each operation and is refused with
  `workspace_held` while another engine holds it. The operations run over a `Workspace`, a
  `FileWorkspace` or a `MemoryWorkspace`. The product supplies each entity's `DocumentFormat` and
  the `Upgrade`s it registers.
- `EndpointRecord` is the daemon's MCP endpoint and local credential. The daemon publishes it in
  its local store's `daemon` journal and retires it at stop; a CLI that names no endpoint
  resolves the daemon through it. `journals.list` lists the `daemon` journal.

## 0.1.0

- First release: `walthamstow-kernel`, `walthamstow-workspace` and `walthamstow-upgrades`,
  extracted from Primrose Hill's engine.
