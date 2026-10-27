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
release finishes it. On Unix, each record a write leaves, and each directory a write creates, is
readable and writable by its owner alone. Every open of one data directory shares one store, whatever path reaches
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

A file that holds neither its source nor its target refuses the publication with the reason
`upgrade_stale`. A completed upgrade's directory may be removed, and so may that of an upgrade
whose first publication was refused before writing a file.

Recovery publishes each upgrade its journal authorises and discards staging no journal
authorises. It reports both by operation id, and refuses an upgrade's directory whose name is not
UTF-8.

## Operations

`walthamstow-ops` contributes Walthamstow's operations to Clapham under the `walthamstow`
namespace. Operations reach Walthamstow through the product's gate, which has decided whether the
principal may run them. Walthamstow does not judge principals.

Every operation runs against the workspace under its lease:

- A daemon holds the workspace lock for as long as it runs.
- An in-process front end takes the lock for each operation, and is refused with
  `workspace_held` while another engine holds it.
- Within one engine, operations on the workspace run one at a time.

A refusal's code is the reason code Walthamstow names it with, under the category every
operation declares for it.

`workspace.upgrade` runs one registered upgrade over each document whose format, as the product
tells it, that upgrade upgrades:

- A document in a format newer than the product knows refuses every upgrade with
  `document_newer`.
- The upgrade replaces documents only while each holds what the upgrade read. Otherwise nothing is
  replaced, and the upgrade is refused with `upgrade_stale`.
- A preview reports a revision naming what the upgrade read. An upgrade given that revision is
  refused with `upgrade_stale` unless its documents still hold what the preview read.
- Over files, the upgrade is journalled as an upgrade's operation: `<upgrade>-<revision>`, the
  revision cut to 16 digits.

`journals.list` lists the records of the publication journal and of each journal the product
names.

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
