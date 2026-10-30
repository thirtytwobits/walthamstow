# Walthamstow

An engine kernel for products built on [Clerkenwell](https://github.com/thirtytwobits/clerkenwell):
the parts of an engine daemon that do not depend on what the product is.

- `walthamstow-kernel`: errors and reason codes, the event bus and daemon lifecycle, clocks,
  durable writes, file helpers, hashes, journal layout, the engine's local records, document names,
  leases, the fenced publication of staged files and collaboration storage.
- `walthamstow-logs`: log documents, which do not merge: a head and an ordered log of records,
  written by one lease holder at a time, and the blobs they name. It holds the `LogStore` and
  `BlobStore` ports, their memory adapters, the file blob store and the document layout it
  shares with the file log store, and the conformance suite every adapter runs.
- `walthamstow-workspace`: a file-backed workspace of typed documents. It covers directories
  within the root, listing and parsing documents, schemas, mounts, the walk of a schema into field
  guidance, each collaborative entity's documents, and the journal of accepted documents still
  waiting for their files.
- `walthamstow-upgrades`: explicit document upgrades. The kernel's publication replaces a
  workspace's files, and an upgrade's journal recovers an interrupted one.
- `walthamstow-ops`: the workspace's status, documents, upgrades and recovery, and the engine's
  journals, as operations contributed to [Clapham](https://github.com/thirtytwobits/clapham), and
  the daemon's endpoint record. The other crates take no Clapham.

`examples/ops` assembles the operations into a daemon and a CLI, and runs Clapham's conformance
harness over them.

A product registers into the kernel. It supplies its entities, validators and events, and returns
its own error, which converts from `KernelError`. Walthamstow builds on Clerkenwell and on no
product or dataflow framework; `cargo xtask boundaries` enforces it.

[docs/contract.md](docs/contract.md) states what Walthamstow guarantees, to whom, and the threat
model it holds to.

## Using it

[docs/using-a-release.md](docs/using-a-release.md) says how to take a release.

## Developing

See [CONTRIBUTING.md](CONTRIBUTING.md). Changes reach `main` through pull requests.
