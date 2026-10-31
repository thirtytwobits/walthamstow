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
- each upgrade's operation directory

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
  document's writes, and `FileLogStore` with a hash of its files. Content restored from outside
  the store carries the revision it had.

Reads:

- `head` returns the format, head, next position and revision, or `None` for a document that does
  not exist.
- `records` returns a page of the records at or after a position, as the document stood at a
  revision, and is refused with `log_stale` once the document is at another revision or at none.
  `list` returns a page of an entity's documents, in resource order, after a resource. A page of
  none is refused as an invalid request.
- A read never creates, repairs, truncates or upgrades anything. `FileLogStore` may record a
  stamp in the engine's local records, which changes no document.
- A read reports a broken document by a `LogFault`. `list` carries the fault, and `head` and
  `records` refuse with it as their reason: `log_missing` when data the store needs is absent,
  naming each part; `log_invalid` when what is stored breaks this contract; `log_inaccessible`
  when storage refused the read. A broken document is listed with its fault, never left out, and
  never rebuilt from another copy.
- A record's blob references are returned as stored. A log read never consults a blob store.

`MemoryLogStore` keeps log documents in memory, and `FileLogStore` as files people can read and
edit. Every adapter passes `walthamstow_logs::conformance`, behind the `conformance` feature.

## Blobs

A blob is bytes a record or a head names: kept once per document under a kind the product
registers, and named by its kind and the SHA-256 of its bytes as `<kind>/<sha256>`. A kind is
registered once per store, with a name of lowercase ASCII letters, digits, `-` and `_` that is no
device name, and an extension of lowercase ASCII letters and digits. A blob of an unregistered kind
is refused, on a read as on a write, so a product that retires a kind can no longer read its blobs.

- A put and a removal run inside `Leases::fence` for the document the lease names, and are refused
  with `lease_lost` otherwise. Putting bytes the store already holds changes nothing.
- A put comes before the write that names its blob, and may precede the document's creation. A
  document exists when its head does; blobs held for a document that does not exist are orphans.
- Every read checks the bytes against the digest that names them. Bytes that do not match are
  refused with `blob_corrupt` and never returned. `get` returns `None` for an absent blob. `check`
  reports each blob it is given that is absent (`blob_missing`) or corrupt, as a `BlobFault`, in
  the order given. A fault is never repaired: a blob stays absent or corrupt until its bytes are
  put again.
- `list` pages a document's blobs by kind, then digest. A page of none is refused as an invalid
  request. The store does not know what names a blob: a caller removes, under the lease, the
  blobs `list` returns that no head or record names.

`FileBlobStore` keeps each blob as the file
`<root>/<entity directory>/<resource directory>/<kind>/<sha256>.<extension>`, which people open
like any other file of its type. The document directory is `DocumentLayout`'s, shared with the
file log store:

- The layout holds its root as a `cap-std` directory, and every operation goes through it. A
  path or a link that would leave the root fails. Within the root, a directory reached through a
  link is used.
- The product registers each entity with the layout, with the files the log store keeps in its
  documents. The entity directory is the entity's name in lowercase, with every character except
  ASCII letters, digits, `-` and `_` written as `-`. A name that is empty or a device name is
  refused, and so is a second entity whose directory is a registered entity's, since the two
  would share their documents. A document of an entity not registered is refused.
- The resource directory is the resource percent-encoded: every byte but lowercase ASCII
  letters, digits, `-` and `_` is written as `%` and two uppercase hex digits, and the first byte
  is written so when the result would be a device name Windows reserves. Two resources never
  share a directory, whatever a file system folds, and the resource reads back from the name. A
  resource that is empty, or whose name is longer than Windows allows, is refused.
- A kind whose name matches a file the log store names in a document, compared case-folded, is
  refused when the store is built.
- A put is a durable write. A file present with the right bytes is settled, its contents and its
  entry, so a put that succeeds has left a durable blob whatever an earlier put or the owner
  left. A file present with the wrong bytes is replaced: the one place the store writes over what
  it finds broken, since the writer supplies the bytes.
- A blob is read only from a regular file. A link or any other entry under a kind directory is
  refused unopened. A file a put staged and did not finish is passed over.

`MemoryBlobStore` keeps blobs in memory. Every adapter passes the blob section of
`walthamstow_logs::conformance`.

## File log documents

`FileLogStore` keeps each log document in its `DocumentLayout` document directory as two files,
which the product names for each entity with `LogFiles`, such as `head.json` and `log.ndjson`.
The layout registers both for the entity.

- The head file is one JSON object: `format`, `incarnation`, `writes`, `next_position` and `head`,
  the product's value. `writes` counts the store's writes to the document, so no two writes leave
  the same head file.
- The log file holds one record per line, each a `StoredRecord` as JSON.
- A document exists when its head file does. A directory holding neither file is no document; one
  holding one of the two is `log_missing`, naming the absent file. Every other entry in the
  directory is the product's, which the store neither reads nor changes, except that removing a
  document removes its directory whole, its blobs with it.
- A head or log file that is not a regular file is refused unopened, with `log_inaccessible`, and
  so is a document whose directory storage refuses. `list` and `verify` resolve a document
  directory reached through a link as the layout does; one that leaves the root is listed with
  `log_inaccessible`.

A revision is the head file's incarnation, tagged with the SHA-256 of the head file's digest and
the chain. The chain is the SHA-256, over the committed lines in order, of the previous chain
value and the line without its newline. Two copies of the same bytes give the same revision. The
revision is of the bytes as they are, so a checkout that converts line endings changes it.

The files are the record:

- A line counts once the head's `next_position` is past it, whoever wrote it. Reads pass over the
  lines no head counts, and over a final line without a newline that is not a record.
- Every other line must be a record, and positions must increase along the log, or the document
  is `log_invalid`. A line a person edits or adds is read as written once the head counts it.
- A commit drops the lines no head counts, appends its lines and syncs the log, then replaces the
  head file durably. Its cost depends on its size, not the log's.
- `create`, `rewind`, `replace` and `remove` stage their files in `staging/log-operations/<id>/`,
  and a record in the `log-operations` journal authorises their [publication](#publication) over
  the document's files.
- A write that finds the files at a revision other than its base is refused with `log_stale`, so
  an edit, a `git checkout` or a copy made since the writer read the document is never written
  over.
- A commit is remembered for a retry before its head file lands, and forgotten when it fails
  without landing. Any other write is remembered once its publication is finished. A retry given
  a revision first makes the document's files durable, since the write may have landed and
  failed before it made them so.

Opening the store recovers. It finishes each publication a record authorises, discards one whose
file changed after it was authorised, keeping the change, and discards staging no record
authorises. A write that leaves its publication authorised stops the document: every later write
to it is refused until the store is opened again and recovery finishes or discards the
publication. Reads go on.

The store learns a revision through the stamp cache, the `log-stamps` journal of the engine's
local records. Per document, it records a stamp of the log file, the head's next position and
what the committed lines gave. A stamp is device, inode, length, modification time and change
time on Unix, and length and modification time on Windows.

- A read whose log file matches its stamp, under the same next position, takes the recorded
  result without reading the log. Any other read hashes the log.
- A stamp is recorded only when the log file last changed at least the settled age before it was
  taken, on the store's `WallClock`, since a file can change again within its clock's resolution
  without changing its stamp. The product sets the settled age at or above that resolution.
- Losing the cache costs one hash per document. A record of another format is passed over.
  `verify` hashes every document of an entity and records the stamps of those that have settled.

`records` reads the log from its start. Paging bounds the records returned, not the bytes read.

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

## Publication

A publication replaces live files with staged ones. It holds the live and staged roots as
`cap-std` directories, and every file resolves through them. A path or a link that would leave
either root fails. A live file reached through a linked directory is refused, since two paths
through one can name one file.

- Each replacement names its file once, compared case-folded, by normal components only. Two that
  differ only in case are refused with the reason `publication_paths_collide` before anything is
  written.
- Each replacement carries the hash its live file held when staged, or none when there was no
  file, and its target: the staged file's hash, or none when the publication removes the file.
- Every replacement is checked before any file is written. A live file that holds neither its
  source nor its target refuses the publication with the reason `publication_stale`. A first
  publication also refuses a file already at its target; publishing again accepts it, since an
  interrupted publication may have written it.
- A live or staged file must be a regular file, and a staged file must match its target hash.
- Each file is written durably, and each removal is made durable.

Within one engine, a process-wide lock holds each publication from its first check to its last
write, so two publications from one source cannot both pass their fences.

## Upgrades

An operation id is lowercase ASCII letters, digits, `-` and `_`, and is not a device name. An
upgrade works in `journals/workspace-upgrades/<operation id>/`:

- `prepared/` holds the files it publishes
- `original/` holds the files they replace
- `operation.json` authorises publication

An upgrade publishes its files from `prepared/` into the workspace root as a
[publication](#publication), and reaches its originals and staged files through its operation
directory. An upgrade names a publication's refusals as its own: two files that differ only in
case are refused with the reason `upgrade_paths_collide`, and a stale file with `upgrade_stale`.
Recovery publishes again, so it accepts a file already at its target.

A completed upgrade's directory may be removed, and so may that of an upgrade
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
