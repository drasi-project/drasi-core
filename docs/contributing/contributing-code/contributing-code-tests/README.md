# Running tests

## Types of tests

We apply the [testing pyramid](https://martinfowler.com/articles/practical-test-pyramid.html) to divide our tests into groups for each feature.

- Unit tests: exercise functions and types directly
- Integration tests: exercise features working with dependencies
- Architecture checks: reject the selected dependency directions with cargo-bylaw

## Unit tests

Unit tests live within the crate of each component and can be run with the following command:

```sh
cargo test
```

We require unit tests to be added for new code as well as when making fixes or refactors in existing code. As a basic rule, ideally every PR contains some additions or changes to tests.

Unit tests should run with only the [basic prerequisites](../contributing-code-prerequisites/) installed. Do not add external dependencies needed for unit tests, prefer integration tests in those cases.

## Integration tests

The [shared-tests](../../../../shared-tests/) directory contains a suite of scenario tests that spin up continuous queries, push changes though them and assert the results.  

Running `cargo test` in this directory will run these tests against the in-memory storage implementation.

These scenarios are also shared by the Garnet/Redis and RocksDb storage implementations.  

Running `cargo test` in the `index-garnet` folder will run them against a real Garnet/Redis instance. By default, it will try to use the connection string of `redis://127.0.0.1:6379`, but this can be overridden by setting the `REDIS_URL` environment variable.

Many plugin integration tests are marked `#[ignore]` so default `cargo test` (and the unit-test CI workflow) can run without external services. CI discovers ignored tests that start containers (`testcontainers` or shared Redis/k3s helpers) and runs them from `.github/workflows/test-integration.yml`. Adding a new crate with that pattern is enough; the workflow does not keep a package allowlist. Locally:

```sh
cargo test -p drasi-source-postgres --test integration_tests -- --ignored --nocapture
cargo run -p xtask -- list-integration-test-matrix
```

Running `cargo test` in the `index-rocksdb` folder will run them against a real RocksDb, which is embedded as an in-process library. Running the tests will create a `test-data` directory where the RocksDb files will be stored.

## Run architecture checks

After [optional setup](../contributing-code-prerequisites/#set-up-optional-architecture-checks), run from the repository root:

```sh
cargo bylaw check
```

If you used the isolated installation, prefix the command with `PATH="$HOME/.local/share/drasi-bylaw/bin:$PATH"`.

The [policy](../../../../bylaw.toml) analyzes the library targets of `drasi-ffi-primitives`, `drasi-lib`, `drasi-query-ast`, `drasi-query-cypher`, and `drasi-query-gql`.

| Origin | Forbidden production dependencies |
| --- | --- |
| FFI primitives | Everything except themselves and toolchain crates |
| Embedded runtime | Concrete source, reaction, bootstrap, identity, and secret-store plugins, plus index backends |
| AST | Parsers, engine, function-binding crates, runtime, concrete plugins, and index backends |
| Both parsers | Engine, function-binding crates, runtime, concrete plugins, and index backends |

Plugin-to-runtime dependencies, runtime-to-AST/parser dependencies, and parser-to-AST dependencies remain allowed. The runtime's RocksDB dev-dependency remains allowed. There is no general third-party dependency allowlist outside FFI primitives.

For the same supplemental checks as CI, use Python 3.11 or later:

```sh
python3 .github/scripts/check-architecture.py
python3 .github/scripts/check-architecture.test.py -v
```

Replace `python3` with your newer interpreter if needed. The fixture runner also needs the pinned Bylaw CLI on PATH. It creates temporary workspaces, checks allowed and forbidden dependencies, and removes the fixtures afterward. It does not inject invalid dependencies into this checkout.

Bylaw checks source use and resolved declarations for the current host and default feature graph. The Python helper also checks normal manifest dependencies that are optional, target-specific, renamed, unused, or registry-sourced. This is not semantic coverage of every target or feature combination. These checks do not replace behavioral tests or make query results, scheduling, and external services deterministic.

To add a concrete plugin or index backend, update both its package selector and `external_crates` selector in `bylaw.toml`. Use the library target's Rust crate name for `external_crates`, not a dependency alias. The helper checks those exact-name inventories against Cargo metadata. Static implementations and test plugins count as concrete plugins; `drasi-source-mapping` is a helper and does not. Keep all five rules, origin packages, and complete analysis settings.

Bylaw exits with `0` for success, `1` for rule violations, `2` for configuration errors, and `3` for analysis errors. For structured diagnostics, run `cargo bylaw check --format json`. Fix the dependency direction or build prerequisites rather than allowing incomplete analysis. Report analyzer failures separately from architecture violations.

The standalone [Architecture workflow](../../../../.github/workflows/architecture.yml) runs on every pull request, including drafts, without path filters. It preserves diagnostics and fails if analysis changes `Cargo.lock`. Normal `cargo build`, `cargo test`, lint commands, and hooks do not invoke Bylaw.

After the workflow first passes on GitHub, a maintainer must add its observed `Architecture` check context to the ruleset for `main`. Preserve existing required checks. Adding the workflow file alone does not make the check required.
