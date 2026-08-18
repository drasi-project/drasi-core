# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-18

### Added

- Initial OpenTelemetry OTLP source (`kind: otel`)
- OTLP/gRPC and optional OTLP/HTTP protobuf receivers
- Graph projection for Service, Metric, Heartbeat, LogEvent, REPORTS, DEPENDS_ON, HEARTBEAT, EMITS
- TTL expiry for DEPENDS_ON edges and LogEvent nodes
- Optional WAL durability and inbound bearer/basic auth
