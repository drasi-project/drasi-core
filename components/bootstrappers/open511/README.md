# drasi-bootstrap-open511

Open511 bootstrap provider for Drasi.

This provider fetches a full Open511 snapshot and emits initial insert events so queries can start with current road-event state before streaming updates arrive from the source.

## Builder Example

```rust
use drasi_bootstrap_open511::Open511BootstrapProvider;

let provider = Open511BootstrapProvider::builder()
    .with_base_url("https://api.open511.gov.bc.ca")
    .with_status_filter("ACTIVE")
    .build()?;
```

Use this provider with `Open511Source::builder(...).with_bootstrap_provider(provider)`.
