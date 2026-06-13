# MQTT Reaction Integration Tests

These integration tests usese a real Mosquitto instance via testcontainers.

## Running tests

```bash
cargo test -p drasi-reaction-mqtt --test integration_tests -- --ignored --nocapture
```

The tests are ignored by default because they require Docker.
