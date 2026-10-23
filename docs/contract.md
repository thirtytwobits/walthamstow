# Contract and threat model

What Walthamstow guarantees, to whom, and under which assumptions. A failure to meet this
document is a defect, fixed with a test. Anything else is an issue, or a change to this document.

## Trust

The **product** is the engine built on Walthamstow, and it is trusted. Walthamstow checks what the
product configures for mistakes, not for hostility. That configuration is:

- the data directory
- the workspace root and its configured directories
- the write durability
- journal and staging names
- entity specs and validators
- the collaboration root and records directory

**Clerkenwell** is trusted. It passes back to the storage port only the sources that `source` and
`sources` returned.

**Input** is untrusted. It reaches Walthamstow through the product:

- resource ids
- operation ids
- mount-relative paths
- document content
- authored updates
- the files people edit in the workspace

Walthamstow refuses input that it cannot store, name or read safely. It never lets input move a
read or a write outside the directory it belongs in.

## Ownership

One engine owns a workspace and a data directory. The workspace lock holds the root against a
second engine. Within one engine, process-wide locks serialise publications, directory creation
and each record of a data directory's local store. Two engines sharing a workspace or a data
directory are outside this contract.

The data directory and the workspace belong to whoever runs the engine. Walthamstow checks a path
when it opens, resolves or names it. After that, a change the owner makes is the owner's: for
example, a directory replaced by a link while the engine runs. Walthamstow does not defend
against it.

## The file system

Configured paths are used as given, links in them included. On macOS, `/var` and `/tmp` are
links.

Walthamstow refuses a link wherever it names an entry itself:

- `journals/`, each journal, and every entry in a journal
- `staging/` and each staging directory
- each file the file canonical store keeps a part in, and each directory between it and its
  entity's directory
- the collaboration cache root that `prepare_cache_root` is given, and each entity directory
- publication marks
- each upgrade's operation directory, its originals and its staged files

A file Walthamstow reads as state must be a regular file. A link, FIFO, socket or device found
there is refused unopened.

A configured workspace directory must meet all of these:

- it is a normalised relative path in ASCII
- every component is a name Windows allows: no reserved character or device name, no trailing
  dot or space
- it stays within the root, with in-root links resolved at resolution
- it overlaps no other configured directory, compared case-folded
- every part of it that exists is a directory

Discovery refuses a link or special file wherever it would find a directory or a document.

Every path Walthamstow turns into text is UTF-8: the collaboration root, its records directory,
and each workspace-relative path. Anything else is refused, so text always names the file it
came from.

Linux, macOS and Windows on NTFS are supported. Names Walthamstow generates or compares assume a
file system that may fold case.

## Durability

`WriteDurability::Device` writes through to the storage device. It uses `F_FULLFSYNC` on Apple
platforms, `fsync` on other Unix systems, and `FlushFileBuffers` on Windows.
`WriteDurability::System` writes at least to the operating system.

A durable write goes through a temporary file, a sync, a rename and a sync of the directory. It
is durable at the chosen level when it returns success.

An operation that returns an error may have left its rename visible but unsynced. Repeating the
operation settles it before reporting.

Journals record what an operation is authorised to do. Recovery finishes only what a journal
authorised.

## Local records

A `LocalStore` keeps the engine's local records, each under a journal and a path of names. A name
is any text but the empty text. A write replaces or removes a record only over the record it names
by its tag. A successful write is durable when it returns; a write that fails may still have taken
effect. Reading never creates, repairs or upgrades a record.

`FileLocalStore` owns each journal's directory, `journals/<journal>/`. It keeps each record in a
file named by the SHA-256 of its key's names, holding those names on its first line and then the
record's bytes, so a key never becomes a path. A record's tag is the SHA-256 of its bytes. Within
one engine process, every operation on a record holds that record's lock, and a listing holds out
every write, so it sees the records as they were at one moment. A write stages a file beside the
record and renames it into place, and a read settles what it finds, a record or none. Any other
entry in a journal's directory is refused, including what an earlier release left there: that
release finishes it. Every open of one data directory shares one store, whatever path reaches
the directory: a link, or a name in another case on a file system that folds case.

## Collaboration storage

An envelope is kept at `<root>/<entity directory>/<sha-256 of the resource id>.json`:

- The entity directory is the entity's name in lowercase, with every character except ASCII
  letters, digits, `-` and `_` written as `-`. Reserved and device names are refused.
- The directory's `entity` file names the entity it holds. A second entity that maps to the same
  directory is refused.
- Writes take a lock file per document.
- The records are `audit.ndjson` and `quarantine/`. They are kept beside the envelopes, or under
  the records directory when one is given.

A source is the envelope's full path as text. `read` and `stamp` accept only a source that names
an envelope this layout generates.

`compare_and_swap` writes only when the stored version equals the expected one. On a mismatch it
returns `None`, after syncing the entity directory. `compare_and_remove` removes only when the
stored version equals the expected one.

## Leases

A lease is the right to change one document in ways that do not merge. It names its document,
its holder and an epoch that rises with each grant. A store checks the lease in the same atomic
step as the write it fences, and refuses a write under a lease that no longer holds or names
another document.

`FileLeases` grants one lease over every document of a workspace: the workspace lock, held for as
long as the engine runs, under an epoch one past the last hold's, kept in the `leases` journal. `MemoryLeases` grants each
document its own lease, which lapses a term after it was granted or renewed, on the engine's
clock.

## The canonical store

A canonical store keeps collaborative documents as the product's codec renders them, in named
parts. A document renders at least one part, no two whose names fold alike, and none whose name
holds another beneath it. No part's last name is one a durable write stages a file under. Every
store is given the same publication, the version, every accepted operation and the parts, and
keeps what it can of it. Publishing what a store holds changes nothing. `admit` refuses parts a
store cannot keep as a document's, before anything is written.

`FileCanonicalStore` writes each part as a file under its entity's directory. In the
`canonical-files` journal it records the version each document's files render and a digest of
each file. A document's files are those the record names that still hold what the store wrote,
and every file the codec names as the document's. When they are not exactly what the record
names, they were written outside any engine, and the store holds them with no version. Each
entity's files are read and written under one lock, which every store over the entity's directory
in one process shares, whatever the case of the path that names it. It admits no part the codec
names as another document's. A publication checks every part before it writes any, and never
writes over or removes a file the codec names as another document's, whatever the store wrote
there before. Every file the store holds of the document that a publication omits is dropped.
Dropped parts are removed first, with the directories they leave holding nothing but files a
stopped write staged, so a part may take the place of one as a file or a directory, in any case.
A part renamed only in case is renamed in place before it is written, and never onto another
document's file. Discovery passes over hidden entries and the files a stopped write staged.

`MemoryCanonicalStore` keeps each document's version, operations and parts.

## The outbox

A product marks a document before it commits to it. Publishing the document renders its accepted
state with the entity's codec and publishes it to the canonical store under the document's lease,
taken for the write alone. Publications of one document land in the order they read it. A
publication is refused while the store holds operations of the document's history that it lacks:
another holder published them after it read its state. A removal's mark records the version
removed, before the removal and again if a commit landed meanwhile. Publishing the removal
removes the document from the store only when its mark records one, and is refused while the
store holds operations of its history that version lacks. The mark stays until one of these
is true:

- no commit to the document is under way, and the store holds the version accepted now
- its removal is published

A version names the document's incarnation, so a store holding a removed document's history
never clears the mark of a document created again under its name. Each mark is a record in the
`collaboration-publications` journal, and every open over one local store shares one outbox.

## Upgrades

An operation id is lowercase ASCII letters, digits, `-` and `_`, and is not a device name. An
upgrade works in `journals/workspace-upgrades/<operation id>/`:

- `prepared/` holds the files it publishes
- `original/` holds the files they replace
- `operation.json` authorises publication

A publication names each file once, compared case-folded, by normal components only. Every
replacement is checked against the hash its live file held when staged before any file is
written.

## Schema guidance

`schema_describe` walks a document's schema into field guidance. It is guidance, not validation:
validation is `jsonschema`'s, and the walk never fails.

The walk reads these keywords:

- `type`, as a name or a list
- `properties`, `required` and `items` (one schema)
- `enum`, `const` and `description`
- local `$ref`s

A `$ref` beginning with `#` is a percent-decoded JSON Pointer into the same document. A chain of
references is followed to its end and stops at a cycle.

Keywords beside a `$ref` apply at every hop of the chain:

- `type`, `items`, `enum`, `const` and `description`: the nearest one wins
- `properties` and `required`: they accumulate

A recursive definition is walked once, however it is reached.

Any other keyword leaves its field labelled by what the walk can read, or `any`, with nothing
beneath it walked. This covers:

- remote references, `$id`, `$anchor` and `$dynamicRef`
- `allOf`, `anyOf`, `oneOf` and `not`
- conditionals
- `prefixItems`, `patternProperties` and `additionalProperties`
