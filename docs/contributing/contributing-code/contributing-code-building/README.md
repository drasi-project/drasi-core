# Building the code

The root directory is a [Cargo workspace](https://doc.rust-lang.org/cargo/reference/workspaces.html).
Build it with the pinned Rust toolchain and committed root lockfile:

```sh
cargo build --locked --workspace
```

Outputs normally live in `target/`; `CARGO_TARGET_DIR` can relocate them.
Build a specific package with `cargo build --locked -p <package>`.
Update the root `Cargo.lock` deliberately when changing dependencies; do not
delete it to repair a build.

Tests that load plugins need separately built shared libraries:

```sh
make build-test-plugins
```

This uses the same script locally and in CI. It stages each freshly built
library in Cargo's `debug/plugins/` directory, with test fixtures under
`debug/plugins/examples/`. Ordinary Cargo builds may change feature sets and
overwrite files in `debug/`; tests must not load those unstaged outputs.
Do not enable `dynamic-plugin` workspace-wide: dependency-connected plugins
export the same entry-point names and must be built separately.

See [running tests](../contributing-code-tests/) for the complete test command
and service prerequisites, and [code organization](../contributing-code-organization/)
for the workspace layout.