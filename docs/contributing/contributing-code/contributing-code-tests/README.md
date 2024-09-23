# Running tests

## Types of tests

We apply the [testing pyramid](https://martinfowler.com/articles/practical-test-pyramid.html) to divide our tests into groups for each feature.

- Unit tests: exercise functions and types directly
- Integration tests: exercise features working with dependencies

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

Running `cargo test` in the `index-rocksdb` folder will run them against a real RocksDb, which is embedded as an in-process library. Running the tests will create a `test-data` directory where the RocksDb files will be stored.

