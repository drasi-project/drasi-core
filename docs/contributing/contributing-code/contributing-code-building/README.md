# Building the code

The root directory of the repo is a [Cargo Workspace](https://doc.rust-lang.org/cargo/reference/workspaces.html).  This means you can simply run `cargo build` from the root directory to build everything, the output will be in the `target` directory.
If you wish to build a specific component, simply run `cargo build` from the folder of the component.  See [Understanding the drasi-core repo code organization](../contributing-code-organization/) for more information on the specific components.

## Running Clippy

Clippy is Rust's linter that helps catch common mistakes and improve code quality. The repository is configured with workspace-level clippy lints in `Cargo.toml`, so all crates automatically use the same configuration as CI.

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
# From repository root
cargo clippy --all-targets --all-features
```

Or use the Makefile shorthand:

```bash
make clippy
```

### Running clippy on a specific crate

To run clippy on a specific crate, navigate to the crate directory:

```bash
# Navigate to the crate directory
cd core
cargo clippy --all-targets --all-features

# To automatically fix issues where possible
cargo clippy --fix --all-targets --all-features
```

Example for different crates:

```bash
cd query-ast && cargo clippy
cd ../core && cargo clippy --fix
cd ../query-cypher && cargo clippy --all-targets
```

### Available crates

- `core` - Core continuous query engine
- `query-ast` - Abstract Syntax Tree definitions
- `query-cypher` - Cypher query parser
- `query-gql` - GQL query parser
- `functions-cypher` - Cypher function implementations
- `functions-gql` - GQL function implementations
- `index-rocksdb` - RocksDB index implementation
- `index-garnet` - Garnet index implementation
- `middleware` - Middleware components
- `shared-tests` - Shared test utilities
- `query-perf` - Performance testing
- `examples` - Usage examples

### Clippy configuration

The clippy configuration is defined in the workspace `Cargo.toml` and automatically inherited by all crates:

```toml
[workspace.lints.rust]
warnings = "deny"       # Treat all warnings as errors
unused = "allow"        # Allow unused code

[workspace.lints.clippy]
print_stdout = "warn"       # Warn on print!/println! usage
unwrap_used = "warn"        # Warn on .unwrap() usage
module_inception = "allow"  # Allow module inception
ptr_arg = "allow"           # Allow pointer arguments
type_complexity = "allow"   # Allow complex types
```

Additional clippy configuration is in `clippy.toml` at the repository root:
- `allow-print-in-tests = true` - Allows print statements in tests
- `allow-unwrap-in-tests = true` - Allows unwrap in tests