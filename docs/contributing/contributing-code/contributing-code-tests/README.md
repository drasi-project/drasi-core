# Running tests

## Types of tests

We apply the [testing pyramid](https://martinfowler.com/articles/practical-test-pyramid.html) to divide our tests into groups for each feature.

- Unit tests: exercise functions and types directly
- Integration tests: exercise features working with dependencies

## Unit tests

From the repository root, the normal local and primary CI entry point is:

```sh
make test
```

It runs the foundational contract/ABI/SDK library tests first, prepares the
plugin libraries once, then runs the workspace and every Host SDK target.
The Host SDK is tested separately with `test-support,registry,fetcher,watcher`.
All Cargo invocations use the root lockfile. A failed prerequisite stops the
next stage; use Cargo's reported package/target to investigate it before
working on higher layers.

The full workspace needs the [build prerequisites](../contributing-code-prerequisites/)
and a Redis-compatible service at `redis://127.0.0.1:6379` (or `REDIS_URL`).
Individual crate unit tests generally need no external service.
Leave `RUST_LOG` unset for tests that assert logging behavior.

For focused work:

```sh
cargo test --locked -p drasi-core-types --lib
make test-host-sdk
make build-test-plugins
cargo test --locked -p drasi-host-sdk --test native_query_plugin -- --test-threads=1
```

Both `make test` and `make test-host-sdk` prepare the shared libraries.
Direct native-plugin Cargo commands need `make build-test-plugins` first.
The shared preparation script uses Cargo's actual target directory and
platform library naming; native tests load `debug/plugins/`, not ordinary
`debug/` artifacts. Test fixture libraries live in `debug/plugins/examples/`.
Explicit `DRASI_NATIVE_PLUGIN_DIR`, `DRASI_HOST_TEST_PLUGINS_DIR` and fixture-path
overrides remain available for intentional alternative builds; unset stale
overrides for a normal run.

Additional supported feature combinations use ordinary Cargo commands in
`.github/workflows/test.yml`. Run the relevant matrix command locally when
changing those features. There is no separate runtime-profile runner,
test-name inventory, qualification ledger or calendar-based exception gate.

Unit tests live within the crate of each component and can be run with the following command:

```sh
cargo test --locked
```

We require unit tests to be added for new code as well as when making fixes or refactors in existing code. As a basic rule, ideally every PR contains some additions or changes to tests.

Unit tests should run with only the [basic prerequisites](../contributing-code-prerequisites/) installed. Do not add external dependencies needed for unit tests, prefer integration tests in those cases.

## Integration tests

The [shared-tests](../../../../shared-tests/) directory contains a suite of scenario tests that spin up continuous queries, push changes though them and assert the results.  

Running `cargo test` in this directory will run these tests against the in-memory storage implementation.

These scenarios are also shared by the Garnet/Redis and RocksDb storage implementations.  

Running `cargo test` in the `index-garnet` folder will run them against a real Garnet/Redis instance. By default, it will try to use the connection string of `redis://127.0.0.1:6379`, but this can be overridden by setting the `REDIS_URL` environment variable.

Container-backed integration tests are marked `#[ignore]` and run separately
from ordinary workspace tests. CI discovers their `testcontainers`, Redis/k3s
helpers or Podman fixtures, including modules declared with `#[path]`, through
the existing `.github/workflows/test-integration.yml` workflow. It installs
Podman for fixtures that use its CLI and prepares plugins when required.
Locally, start a Docker-compatible API (Podman is supported) and run:

```sh
make build-test-plugins
cargo test --locked -p drasi-source-postgres --test native_transactions -- --ignored --nocapture --test-threads=1
cargo run --locked -p xtask -- list-integration-test-matrix
```

Running `cargo test` in the `index-rocksdb` folder will run them against a real RocksDb, which is embedded as an in-process library. Running the tests will create a `test-data` directory where the RocksDb files will be stored.

SQL Server tests require an explicitly configured isolated fixture; see the
[fixture instructions](../../../../components/host-sdk/tests/fixtures/native_mssql/README.md).
With those approved inputs supplied, `make test-native-mssql` selects the
ignored database tests. The
[cross-OS instance-host test](../../../../components/instance-host/README.md#qualified-unix-platforms-and-prepared-network-peer)
also needs a prepared peer. Neither is automatically provisioned by ordinary
CI. Missing prerequisites are not passing evidence.

Some ignored tests are subprocess workers. Run their parent tests, not
`--include-ignored` across the whole workspace. A passing default suite does
not claim those service/platform tests passed.
