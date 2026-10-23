# Walthamstow

An engine kernel for products built on [Clerkenwell](https://github.com/thirtytwobits/clerkenwell):
the parts of an engine daemon that do not depend on what the product is.

- `walthamstow-kernel`: errors and reason codes, the event bus and daemon lifecycle, clocks,
  durable writes, file helpers, hashes, journal layout, the engine's local records, leases,
  collaboration storage and the workspace lock.
- `walthamstow-workspace`: a workspace of typed collaborative documents. It covers each
  collaborative entity's documents, the canonical store they are published to, rendered by the
  product's codec, and the outbox of documents it has not yet acknowledged. For a file-backed
  workspace it covers directories within the root, listing and parsing documents, schemas, mounts
  and the walk of a schema into field guidance.
- `walthamstow-upgrades`: explicit document upgrades. Each document's parts are replaced in the
  canonical store, fenced by what the upgrade read, under a journal that recovers an
  interrupted upgrade.

A product registers into the kernel. It supplies its entities, validators and events, and returns
its own error, which converts from `KernelError`. Walthamstow builds on Clerkenwell and on no
product or dataflow framework; `cargo xtask boundaries` enforces it.

[docs/contract.md](docs/contract.md) states what Walthamstow guarantees, to whom, and the threat
model it holds to.

## Using it

Take the crates from a release tag:

```toml
walthamstow-kernel = { git = "https://github.com/thirtytwobits/walthamstow", tag = "v0.2.0" }
```

Clerkenwell's crates build on a Loro fork, and workspace patches do not propagate to dependents.
A workspace that builds Walthamstow carries the `generic-btree` override in this repository's
`Cargo.toml`.

## Developing

See [CONTRIBUTING.md](CONTRIBUTING.md). Changes reach `main` through pull requests.
