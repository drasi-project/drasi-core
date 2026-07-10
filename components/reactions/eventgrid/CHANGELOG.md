# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0]

### Added

- Initial Azure Event Grid reaction crate.
- Publishes query-result changes to an Event Grid custom topic over HTTP.
- Two wire schemas: CloudEvents 1.0 (default) and native EventGrid.
- Three output formats: packed (default), unpacked, and template (Handlebars
  with per-query metadata as CloudEvent extension attributes).
- Access-key (`aeg-sas-key`) and Microsoft Entra Workload Identity (AAD bearer
  token, scoped to `https://eventgrid.azure.net/.default`) authentication.
- Deterministic (UUIDv5) event ids for downstream deduplication.
- Delivery retry with exponential backoff for transient failures.
- Unit tests and a wiremock-based protocol-target integration test suite.
