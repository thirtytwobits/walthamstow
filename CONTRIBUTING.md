# Contributing

Run `cargo xtask check` before opening a pull request: formatting, the dependency boundaries,
the storage layout, Clippy with warnings denied, the tests and the documentation.

`cargo xtask api` compares each crate's public API with its snapshot in `api/`, using the
toolchain `api/toolchain.json` names:

```bash
rustup toolchain install nightly-2025-08-02 --profile minimal
cargo install cargo-public-api --version 0.50.2 --locked
```

A pull request that changes a public API reviews the snapshot diff and runs
`cargo xtask api --update`.

## Releasing

1. Set the version in `Cargo.toml`, both on the workspace and on each path dependency between
   the crates.
2. Add the version's entry to `CHANGELOG.md`.
3. Merge, then push the tag `vX.Y.Z`. The "Draft release" workflow runs the gate, checks that the
   tag names the version, and creates a draft release with the version's changelog entry as its notes.
4. Publish the draft.
