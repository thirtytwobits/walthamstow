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

[RELEASING.md](RELEASING.md) holds the changelog and release rules and the procedure.
