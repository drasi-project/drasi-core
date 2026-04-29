# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-02-20

### Added

- Initial Grafana Loki reaction implementation
- Template-based log line rendering for ADD, UPDATE, and DELETE operations
- Dynamic label rendering with Handlebars templates
- Integration test using Loki Docker container with testcontainers
- Example app (MockSource -> Query -> Loki reaction) under `examples/lib/loki`
