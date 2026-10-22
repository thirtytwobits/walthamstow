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

- `journals/`, each journal, and each directory and record within one
- `staging/` and each staging directory
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

A `LocalStore` keeps the engine's local records, each under a journal and a path of names. A
write replaces or removes a record only over the record it names by its tag. A successful write
is durable when it returns; a write that fails may still have taken effect. Reading never
creates, repairs or upgrades a record.

`FileLocalStore` keeps the record at `journal/name/…` as the file `journals/journal/name/…` under
the data directory, and tags it with the SHA-256 of its bytes. A write stages a `.tmp` file beside
the record and renames it into place under a lock per record. A read settles what it finds, a
record or none. Every open of one data directory shares one store.

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

## Publication marks

A product marks a document before it commits to it. The mark stays until one of these is true:

- no commit to the document is under way, and its files render the version accepted now
- its removal is published

A version names the document's incarnation, so files rendering a document that was removed and
created again never clear its mark. Each mark is a record in the `collaboration-publications`
journal, and every open over one local store shares one journal.

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
