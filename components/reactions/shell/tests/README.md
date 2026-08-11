# Shell Reaction Integration Tests

These integration tests validate changes detection and commands execution.

## Running tests

```bash
cargo test -p drasi-reaction-shell --test integration_tests -- --ignored --nocapture
```

The tests are ignored by default, but they don't require any special setup, just Unix-like environment.
