# Building the code

The root directory of the repo is a [Cargo Workspace](https://doc.rust-lang.org/cargo/reference/workspaces.html).  This means you can simply run `cargo build` from the root directory to build everything, the output will be in the `target` directory.
If you wish to build a specific component, simply run `cargo build` from the folder of the component.  See [Understanding the drasi-core repo code organization](../contributing-code-organization/) for more information on the specific components.

## Running Clippy

Clippy is Rust's linter that helps catch common mistakes and improve code quality. The CI workflow runs clippy with specific flags to ensure code quality. You can run clippy locally with the same configuration as CI.

### Prerequisites

Some crates require the `jq` library. On Ubuntu/Debian:

```bash
sudo apt-get update
sudo apt-get install -y jq libjq-dev
export JQ_LIB_DIR=/usr/lib/x86_64-linux-gnu
```

For the best compatibility with CI, use Rust 1.83.0:

```bash
rustup update 1.83.0 && rustup default 1.83.0
rustup component add clippy
```

### Running clippy on all crates

To run clippy on all workspace members at once (as CI does):

```bash
make clippy
```

### Running clippy on a specific crate

To get the same results as CI for a specific crate, you have two options:

**Option 1: Using the shell script**

```bash
./clippy-crate.sh <crate-name>
```

Example:
```bash
./clippy-crate.sh core
./clippy-crate.sh query-cypher
```

**Option 2: Using make**

```bash
make clippy-crate CRATE=<crate-name>
```

Example:
```bash
make clippy-crate CRATE=core
make clippy-crate CRATE=query-cypher
```

### Running clippy on all crates individually

To run clippy on each workspace member one at a time:

```bash
make clippy-all-crates
```

This is useful for identifying which specific crate has clippy warnings, as the output will be organized by crate.

### Available crates

- `core`
- `query-ast`
- `query-cypher`
- `query-gql`
- `functions-cypher`
- `functions-gql`
- `index-rocksdb`
- `index-garnet`
- `middleware`
- `shared-tests`
- `query-perf`
- `examples`

### Clippy configuration

The clippy configuration matches the CI workflow and includes:
- Warnings treated as errors (`-Dwarnings`)
- Warning on use of `print!` and `println!` (`-W clippy::print_stdout`)
- Warning on use of `.unwrap()` (`-W clippy::unwrap_used`)
- Allowing unused code (`-A unused`)
- Allowing module inception (`-A clippy::module_inception`)
- Allowing pointer arguments (`-A clippy::ptr_arg`)
- Allowing type complexity (`-A clippy::type_complexity`)

Additional configuration is in `clippy.toml` at the repository root.