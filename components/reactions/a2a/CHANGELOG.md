# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-09-09

### Features

- Added `drasi-reaction-a2a` with A2A JSON-RPC 2.0 `SendMessage` and `CancelTask`.
- Added durable activation state keyed as `activation:{queryId}:{resultKey}`.
- Added `next_action` activation state machine with terminal update policy (`replace` | `ignore`).
- Added wiremock integration tests for add/update/delete, one-shot messages, terminal replacement, terminal ignore, and replay behavior.
