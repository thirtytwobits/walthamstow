# Changelog

## 0.3.0

- Breaking: `workspace::recover` returns a `Recovery`: the upgrades it published, and those whose
  staging it discarded, by operation id. It refuses an upgrade's directory whose name is not
  UTF-8.
- On Unix, `FileLocalStore` writes each record, and creates each directory, for its owner alone.
  A record an earlier release left readable by others becomes its owner's alone when next
  written.

## 0.2.0

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
  path that reached it. `MemoryLocalStore` keeps records in memory.
- Breaking: `PublicationJournal::open` takes the `LocalStore` its marks are kept in, and every
  open over one store shares one journal. A mark's record is named by its document. Marks an
  earlier release left are refused: that release publishes what they mark.

## 0.1.0 — 2026-10-20

- First release: `walthamstow-kernel`, `walthamstow-workspace` and `walthamstow-upgrades`,
  extracted from Primrose Hill's engine.
