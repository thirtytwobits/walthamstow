# Changelog

## 0.2.0

- Breaking: Walthamstow builds on Clerkenwell 0.10.0.
  - `CollaborativeDocuments::published` and `AcceptedRead` carry a `CollaborationVersion` in
    place of a generation, so files rendering a removed document's history never clear the mark
    of a document created again under its name.
  - `authoring_state` takes what the peer holds as a `CollaborationHeld`, `bootstrap_update` takes
    the incarnation of the history it is given, and `remove` returns the version it removed.
  - The collaboration cache keeps envelope format 5. `prepare_cache_root` clears a cache in an
    earlier format, which the product rebuilds from its workspace files.

## 0.1.0 — 2026-10-20

- First release: `walthamstow-kernel`, `walthamstow-workspace` and `walthamstow-upgrades`,
  extracted from Primrose Hill's engine.
