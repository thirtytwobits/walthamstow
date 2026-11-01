# Using a Walthamstow release

Clerkenwell's crates build on a Loro fork, and workspace patches do not propagate to dependents.
A workspace that builds Walthamstow carries the `generic-btree` override in Walthamstow's
`Cargo.toml` at the release it takes.

<!-- newsletter:begin consuming · maintained in thirtytwobits/london -->

## Taking a release

Walthamstow publishes nothing to crates.io. Take its crates from a release tag:

```toml
[dependencies]
CRATE = { git = "https://github.com/thirtytwobits/walthamstow", tag = "vX.Y.Z" }
```

Cargo treats each tag as a separate source, so take every Walthamstow crate from the
same tag, and commit `Cargo.lock`. The libraries need Rust 1.95 or later.

`SHA256SUMS` lists the files attached to a release. Check a download with
`sha256sum --check --ignore-missing SHA256SUMS`.

[`london.toml`](https://github.com/thirtytwobits/london/blob/main/london.toml) names the release
of each piece that the London Project's examples build against.

<!-- newsletter:end consuming -->
