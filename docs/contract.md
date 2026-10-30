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

One engine owns a workspace and a data directory. `FileLeases` holds the workspace lock on the
root against a second engine for as long as it is open. Within one engine, process-wide locks serialise publications, directory creation
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

## Leases

A lease grants one holder a document, named by a `DocumentId` of its entity and resource, until it
is released or lapses. While it stands, another holder is refused the document. A holder that
acquires a document it holds is given its lease again, renewed. Each grant's epoch is later than
every grant before it in the process. A lease stands only in the process that granted it.

A fenced write runs while its lease stands, and holds the document against every other grant until
it returns. A write under a lease that was released, has lapsed or was granted by another set of
leases is refused with `lease_lost`, unrun.

`FileLeases` grant leases that never lapse. `MemoryLeases` grant leases that lapse a term after
their grant or last renewal, on the clock they are given.

## Log documents

A log document does not merge. It is written by one lease holder at a time, and the order of its
records is the record. It is:

- a head: one JSON value, replaced whole
- a format: a number the product gives, which the store returns and never interprets
- a log of records, appended in order

The store gives each record its position. Positions increase along the log and need not be
contiguous: a rewind leaves a gap. A position is never given twice in one document, even after a
rewind removes it. A record carries its author, a Clerkenwell principal as the writer names it,
the time the store recorded it on the `WallClock` it is given, the blobs it names and its payload.
The store reads neither the head nor a payload, and never follows a blob reference.

An image, given to `create` and `replace`, holds a format, a head and records with their
positions, which increase along the log. Its next position is one past the highest of them, or the
first position when there are none. `replace` keeps the document's next position unless the
image's is beyond it. `rewind` removes the records from a position before the next one onwards,
then applies a commit, which may be empty. A commit that is not a rewind carries records, a head or
both.

Writes:

- Each write runs inside `Leases::fence` for the document its lease names, and is refused with
  `lease_lost` otherwise.
- A write names the revision it read. A write over any other revision is refused with `log_stale`.
  `create` names none, and is refused with `log_stale` while the document exists. A removed
  document is at no revision, so a write over it is refused with `log_stale`.
- A write carries an id its writer chooses. The store keeps the lease, id and base of its last
  write to a document, through the document's removal. A write under that lease that repeats the
  id and base is a retry: it is given what that write gave, a revision or, to a removal, nothing
  more, and writes nothing again. A write under another lease is judged by its base, whatever its
  id. A removal that repeats another write's id and base, or a write that repeats a removal's, is
  refused with `log_stale`. A lease stands only in the process that granted it, so what a retry
  needs is kept in memory.
- A write is checked for its lease, then whether it is a retry, then its base, then what it
  carries. A write that is refused changes nothing.
- The store never gives a revision twice, including across a document's removal and re-creation:
  a revision carries the document's incarnation, which `create` chooses, and a tag the adapter
  chooses so that it never repeats one. `MemoryLogStore` tags a revision with a count of the
  document's writes. Content restored from outside the store carries the revision it had.

Reads:

- `head` returns the format, head, next position and revision, or `None` for a document that does
  not exist.
- `records` returns a page of the records at or after a position, as the document stood at a
  revision, and is refused with `log_stale` once the document is at another revision or at none.
  `list` returns a page of an entity's documents, in resource order, after a resource. A page of
  none is refused as an invalid request.
- A read never creates, repairs, truncates or upgrades anything.
- A read reports a broken document by a `LogFault`. `list` carries the fault, and `head` and
  `records` refuse with it as their reason: `log_missing` when data the store needs is absent,
  naming each part; `log_invalid` when what is stored breaks this contract; `log_inaccessible`
  when storage refused the read. A broken document is listed with its fault, never left out, and
  never rebuilt from another copy.
- A record's blob references are returned as stored. A log read never consults a blob store.

`MemoryLogStore` keeps log documents in memory. Every adapter passes
`walthamstow_logs::conformance`, behind the `conformance` feature.

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

`CollaborativeDocuments` marks a document before it commits to it. A commit made to the store by
another path, such as `clerkenwell-ops`, is not marked. The mark stays until one of these is true:

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

A publication names each file once, compared case-folded, by normal components only. An upgrade
naming two files that differ only in case is refused with the reason `upgrade_paths_collide`
before anything is written. Every
replacement is checked against the hash its live file held when staged before any file is
written.

A file that holds neither its source nor its target refuses the publication with the reason
`upgrade_stale`. An upgrade's first publication also refuses a file already at its target;
recovery accepts it, since an interrupted publication may have written it. A completed upgrade's directory may be removed, and so may that of an upgrade
whose first publication was refused before writing a file. Removing the latter removes its journal
durably before the files it names, so a removal that stops midway leaves staging that recovery
discards.

Recovery publishes each upgrade its journal authorises and discards staging no journal
authorises. It reports both by operation id, and refuses an upgrade's directory whose name is not
UTF-8.

## Operations

`walthamstow-ops` contributes Walthamstow's operations to Clapham under the `walthamstow`
namespace. Operations reach Walthamstow through the product's gate, which has decided whether the
principal may run them. Walthamstow does not judge principals.

Every operation runs against the workspace under its lease:

- A daemon holds the workspace lock, through its `FileLeases`, for as long as it runs.
- An in-process front end takes the lock for each operation, and is refused with
  `workspace_held` while another engine holds it.
- Within one engine, operations on the workspace run one at a time.

A refusal's code is the reason code Walthamstow names it with, under the category every
operation declares for it.

`workspace.upgrade` runs one registered upgrade over each document whose format, as the product
tells it, that upgrade upgrades:

- While an upgrade a journal authorises is still to finish, every upgrade is refused with
  `upgrade_pending`, its preview included, until recovery finishes it.
- A document in a format newer than the product knows refuses every upgrade with
  `document_newer`.
- The upgrade replaces documents only while each holds what the upgrade read. Otherwise nothing is
  replaced, and the upgrade is refused with `upgrade_stale`.
- A preview reports a revision naming what the upgrade read. An upgrade given that revision is
  refused with `upgrade_stale` unless its documents still hold what the preview read.
- Over files, the upgrade is journalled as an upgrade's operation: `<upgrade>-<revision>`, the
  revision cut to 16 digits.

Over files, a document is named by its path relative to the root, its components joined by `/`.
A document whose name holds a backslash is refused, since that path would name another file.

`journals.list` lists the records of the publication journal, the daemon's journal and each
journal the product names.

The daemon publishes its endpoint record, its MCP endpoint and a local credential, in the `daemon`
journal of its local store when it starts, over any record a stopped daemon left there. It
retires the record when it stops, removing only the record it published: each publication is
stored with an id of its own, so a later publication of the same endpoint is never retired. A CLI calls the
endpoint a flag or the environment names, and otherwise the one the record names.
`FileLocalStore` keeps the record under `journals/daemon/`. An engine with no disk is configured
with its endpoint.

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
