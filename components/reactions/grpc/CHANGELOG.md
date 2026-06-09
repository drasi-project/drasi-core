# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - Unreleased

### Breaking Changes

- **Proto file cleanup.** `proto/drasi/v1/common.proto` is removed and
  `proto/drasi/v1/reaction.proto` is now the single source-of-truth
  proto file for the reaction. All source-side message types (`Element`,
  `Node`, `Relation`, `SourceChange`, `ChangeType`, `BootstrapRequest`,
  `BootstrapResponse`, `ElementReference`, `ElementMetadata`) and the
  unused `ReactionService` RPCs (`StreamResults`, `Subscribe`,
  `HealthCheck`) along with their request / response messages have been
  removed — they were never referenced by the reaction. The retained
  surface is `service ReactionService` with the single `ProcessResults`
  RPC plus the `ProcessResultsRequest`, `ProcessResultsResponse`,
  `QueryResult`, `QueryResultItem`, and `QueryResultItemType` messages.
- **Proto envelope reshape.** `QueryResultItem` is now
  `{ item_type, row_signature, before, after }`:
  - Removed the string `type` field and the `data` field. `data` was a
    duplicate of `after` / `before` for ADD/UPDATE/DELETE; on the wire
    the row state is carried directly by `before` and `after`.
  - Added `item_type` (`QueryResultItemType` enum) replacing the string
    `type`. Eliminates the prior `"ADD"` / `"aggregation"` casing
    inconsistency on the wire.
  - Added `row_signature` (uint64) as a first-class field on every item.
  - `before` and `after` carry the row state on either side of the
    change: `ADD` → only `after`; `DELETE` → only `before`; `UPDATE`
    → both.
- **Enum surface narrowed to 3 ops.** `QueryResultItemType` no longer
  has `AGGREGATION` or `NOOP` variants:
  - Aggregation results surface as `item_type == UPDATE` on the wire
    (matches the RabbitMQ and Azure Storage reactions).
  - Noop results are dropped at the runner level and never appear in
    `QueryResult.results`.
- **Templating model redesigned to per-row.** Templates reshape the row
  content that lands in `before` / `after`, replacing the previous
  separate `payload` field (which has been removed):
  - The `added` template runs once on the new row for ADD → output goes
    into `after`.
  - The `deleted` template runs once on the old row for DELETE → output
    goes into `before`.
  - The `updated` template runs **twice**, independently, for UPDATE —
    once with the before-row → `before`, once with the after-row →
    `after`. Aggregation reuses the same path.
  - New template context variables: `row` (the row being rendered),
    `query_id`, `operation`, `side` (`"before"` or `"after"`). The
    previous `before` / `after` / `data` context variables are removed.
  - Render failure (template parse error, non-JSON output, missing
    field) falls back to the **raw row state** for that field. Events
    are never dropped.
- **`config_version` bumps from `2.0.0` to `3.0.0`** and the crate
  version bumps to `0.5.0`. Receivers compiled against the prior schema
  must regenerate their stubs.
- **Configured `metadata` is now also propagated as gRPC request
  headers.** Previously the configured `metadata` map only populated
  `ProcessResultsRequest.metadata` (a body field). It is now
  additionally set via `tonic::Request::metadata_mut().insert(...)`,
  matching the README's "authentication or routing headers" framing.
  The body field is preserved. Entries with invalid header names /
  values are skipped from the headers with a warning and still ride
  in the body field.

### Features

- Added throughput-aware **adaptive batching** mode (`batching.mode: "adaptive"`)
  using the shared `AdaptiveBatchConfig`.
- Added optional **Handlebars output templates** via `outputTemplates`
  (`defaultTemplate` + per-query `routes`), with `added`/`updated`/`deleted`
  template specs. Rendered output replaces the raw row content in the
  corresponding `before` / `after` field; render failures fall back to the
  raw row state.
- New typed builder API (`GrpcReaction::builder(...)`) with
  `with_fixed_batching`, `with_adaptive_batching`, `with_output_templates`,
  per-field adaptive setters (`with_min_batch_size`, `with_max_batch_size`,
  `with_window_size`, `with_batch_timeout_ms`), and `with_metadata`
  convenience methods.
- OpenAPI descriptor (`config_version = "3.0.0"`) with named sub-schemas for
  `BatchingConfig`, `OutputTemplates`, `QueryConfig`, and `TemplateSpec`, plus
  `SchemaUiAnnotator` groupings for management UIs.

### Miscellaneous Tasks

- Refreshed copyright headers to 2026.
- Expanded `src/tests.rs` and `src/templates.rs` test coverage with
  per-op raw-vs-templated assertions, UPDATE both-sides-rendered
  assertions, Aggregation-as-UPDATE assertion, Noop-unreachable
  contract pin, exact-id routing regression guard, and `row_signature`
  propagation tests.
- Added `tests/mock_server.rs` and `tests/integration_tests.rs` with 14
  end-to-end scenarios that push real `QueryResult` events through the
  reaction to an in-process tonic mock `ReactionService`, asserting
  exact wire shape of every `QueryResultItem` plus gRPC headers.
- Unified `drasi-reaction-grpc` and the previously separate
  `drasi-reaction-grpc-adaptive` crate into a single reaction. The
  `grpc-adaptive` crate has been removed from the workspace.

<!-- generated by git-cliff -->

## [0.4.0] - 2026-05-22

### Breaking Changes

- Unified `drasi-reaction-grpc` and the previously separate `drasi-reaction-grpc-adaptive` crate into a single reaction. The `grpc-adaptive` crate has been removed from the workspace.
- Configuration schema redesigned: top-level fields are now camelCase, and the batching strategy is selected via a discriminated `batching: { mode: "fixed" | "adaptive", ... }` block. There is no backward compatibility with the previous configuration shape.

### Features

- Added throughput-aware **adaptive batching** mode (`batching.mode: "adaptive"`) using the shared `AdaptiveBatchConfig`.
- Added optional **Handlebars output templates** via `outputTemplates` (`defaultTemplate` + per-query `routes`), with `added`/`updated`/`deleted` template specs. Render failures fall back to the raw payload — events are never dropped.
- New typed builder API (`GrpcReaction::builder(...)`) with `with_fixed_batching`, `with_adaptive_batching`, `with_output_templates`, and `with_metadata` convenience methods.
- OpenAPI descriptor (`config_version = "2.0.0"`) with named sub-schemas for `BatchingConfig`, `OutputTemplates`, `QueryConfig`, and `TemplateSpec`.

### Miscellaneous Tasks

- Refreshed copyright headers to 2026.
- Added `src/tests.rs` covering builder defaults, custom values, YAML round-trips for both batching modes, and descriptor schema bundling.

<!-- generated by git-cliff -->

## [0.3.3] - 2026-06-03

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib, drasi-plugin-sdk

<!-- generated by git-cliff -->

## [0.3.2] - 2026-05-21

### Features

- Secret store plugin system with file, keyring, and Azure Key Vault providers ([#460](https://github.com/drasi-project/drasi-core/issues/460))

### Miscellaneous Tasks

- Normalize Apache-2.0 license headers across all .rs files ([#462](https://github.com/drasi-project/drasi-core/issues/462))

<!-- generated by git-cliff -->

## [0.3.1] - 2026-05-20

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib, drasi-plugin-sdk

<!-- generated by git-cliff -->

## [0.3.0] - 2026-05-12

### Bug Fixes

- Lossless config persistence — preserve ConfigValue envelopes, camelCase keys, and passwords ([#422](https://github.com/drasi-project/drasi-core/issues/422))

### Miscellaneous Tasks

- Release ([#446](https://github.com/drasi-project/drasi-core/issues/446))

<!-- generated by git-cliff -->

## [0.2.13] - 2026-05-11

### Bug Fixes

- Lossless config persistence — preserve ConfigValue envelopes, camelCase keys, and passwords ([#422](https://github.com/drasi-project/drasi-core/issues/422))

<!-- generated by git-cliff -->

## [0.2.12] - 2026-04-28

### Miscellaneous Tasks

- Updated the following local packages: drasi-plugin-sdk

<!-- generated by git-cliff -->

## [0.2.11] - 2026-04-27

### Miscellaneous Tasks

- Updated the following local packages: drasi-plugin-sdk, drasi-lib

<!-- generated by git-cliff -->

## [0.2.8] - 2026-03-16

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib, drasi-plugin-sdk

<!-- generated by git-cliff -->
## [0.2.7] - 2026-03-09

### Miscellaneous Tasks

- Updated the following local packages: drasi-plugin-sdk

<!-- generated by git-cliff -->
## [0.2.6] - 2026-02-26

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib

<!-- generated by git-cliff -->
## [0.2.5] - 2026-02-13

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib

<!-- generated by git-cliff -->
## [0.2.4] - 2026-02-11

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib

<!-- generated by git-cliff -->
## [0.2.3] - 2026-02-10

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib

<!-- generated by git-cliff -->
## [0.2.2] - 2026-02-05

### Miscellaneous Tasks

- Updated the following local packages: drasi-lib

<!-- generated by git-cliff -->
<!-- generated by git-cliff -->
