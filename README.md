# Walthamstow

An engine kernel for products built on [Clerkenwell](https://github.com/thirtytwobits/clerkenwell):
the parts of an engine daemon that do not depend on what the product is.

- `walthamstow-kernel`: errors and reason codes, the event bus and daemon lifecycle, clocks,
  durable writes, file helpers, hashes, journal layout, the engine's local records, collaboration
  storage and the workspace lock.
- `walthamstow-workspace`: a file-backed workspace of typed documents. It covers directories
  within the root, listing and parsing documents, schemas, mounts, the walk of a schema into field
  guidance, each collaborative entity's documents, and the journal of accepted documents still
  waiting for their files.
- `walthamstow-upgrades`: explicit document upgrades. A fenced, staged publication replaces a
  workspace's files, and its journal recovers an interrupted one.
- `walthamstow-ops`: the workspace's status, documents, upgrades and recovery, and the engine's
  journals, as operations contributed to [Clapham](https://github.com/thirtytwobits/clapham). The
  other crates take no Clapham.

A product registers into the kernel. It supplies its entities, validators and events, and returns
its own error, which converts from `KernelError`. Walthamstow builds on Clerkenwell and on no
product or dataflow framework; `cargo xtask boundaries` enforces it.

[docs/contract.md](docs/contract.md) states what Walthamstow guarantees, to whom, and the threat
model it holds to.

## Using it

Take the crates from a release tag:

```toml
walthamstow-kernel = { git = "https://github.com/thirtytwobits/walthamstow", tag = "v0.1.0" }
```

Clerkenwell's crates build on a Loro fork, and workspace patches do not propagate to dependents.
A workspace that builds Walthamstow carries the `generic-btree` override in this repository's
`Cargo.toml`.

## Developing

See [CONTRIBUTING.md](CONTRIBUTING.md). Changes reach `main` through pull requests.
