# Walthamstow Agent Instructions

- Walthamstow builds on Clerkenwell and on no product or dataflow framework. A crate never depends
  on a `primrose-*` or `battersea-*` crate; `cargo xtask boundaries` enforces it, and every new
  crate needs a policy entry in `tools/xtask`.
- Run `cargo xtask check` before finishing. Run Cargo commands one at a time.
- A public API change reviews its snapshot diff and runs `cargo xtask api --update`.
- Tests never sleep, set a timer or read the wall clock, and are never retried. Code under test
  that needs time takes its clock as a dependency.
- On-disk formats are compatible across releases. A change to a file a workspace or journal holds
  comes with an explicit upgrade.
- Documentation is en-GB, precise and concise. When something is removed, remove it cleanly,
  without a note that it used to be there.
