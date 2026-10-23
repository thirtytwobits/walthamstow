# Changelog

## 0.2.0 — 2026-10-22

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
- `Leases` grants the right to change a document in ways that do not merge, under an epoch a
  store checks as it writes. `FileLeases` grants one lease over every document: the workspace
  lock, under an epoch one past the last hold's, kept in the `leases` journal. `MemoryLeases`
  grants each document its own lease on the engine's clock.
- `CanonicalStore` keeps collaborative documents as a product's `DocumentCodec` renders them, in
  named parts. Every store is given the same `Publication`: the version, every accepted operation
  and the parts. `FileCanonicalStore` writes the parts as workspace files and records the version
  they render; files edited outside the engine are held with no version, and a publication
  removes those it omits. `MemoryCanonicalStore` keeps the operations as well. `PartName`
  refuses a last name a durable write stages a file under. `admit` refuses parts a store cannot
  keep as a document's; `FileCanonicalStore` refuses a part its codec names as another
  document's.
- Breaking: the outbox replaces the publication journal. `Outbox::open` takes the `LocalStore` its
  marks are kept in, the canonical store, the leases and the holder. A mark's record is named by
  its document. Marks an earlier release left are refused: that release publishes what they mark.
  `CollaborativeDocuments::new` takes `Publishing`: the outbox and the entity's codec.
  `publish` renders and publishes a document, or its removal, under its lease, and clears its mark
  once the store holds the version accepted now. It takes the place of `published`, and refuses
  to publish over operations of the document's history it lacks. `remove` records the version
  it removes in the mark (`PendingCommit::removes`, `Outbox::removed`), and `publish` removes
  from the store only a document its working copy removed, refused the same way. `pull`,
  `parse` and `bootstrap_pulled` read documents back from the canonical store.
  `bootstrap_from_file` goes: `bootstrap` seeds a document and marks it for publication.
- Breaking: upgrades work over the ports. `CanonicalStore::replace` writes a document's upgraded
  parts, which no version names, only while every part the store holds lies `between` the parts
  the upgrade read and those it writes, so repeating a replacement that stopped part way finishes
  it. `workspace::Upgrades` journals each upgrade in the `document-upgrades` journal of the local
  store once the canonical store admits its parts and every document holds only parts it read or
  writes, publishes it document by document after checking every document again, and recovers
  an interrupted one. One upgrade is unfinished at a time. `authorise` is async. An upgrade release 0.1.0 left unfinished in `workspace-upgrades` is refused
  (`refuse_unfinished_earlier_upgrade`): the release that began it finishes it.
  `publication::publish` stays for staged file publications.

## 0.1.0 — 2026-10-20

- First release: `walthamstow-kernel`, `walthamstow-workspace` and `walthamstow-upgrades`,
  extracted from Primrose Hill's engine.
