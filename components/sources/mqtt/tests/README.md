# MQTT Source Integration Tests

These integration tests validate mqtt change detection using a real Mosquitto instance via testcontainers.

## Running tests

```bash
cargo test -p drasi-source-mqtt --test integration_tests -- --ignored --nocapture
```

The tests are ignored by default because they require Docker.
