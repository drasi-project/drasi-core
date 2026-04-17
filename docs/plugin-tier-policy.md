# Drasi Core Plugin Component Tier Policy

* Project Drasi — April 2026

## Introduction

As the Drasi ecosystem grows, the number of plugin components in [drasi-core](https://github.com/drasi-project/drasi-core) is expected to increase significantly through both project-maintained and community-contributed plugins. This policy establishes a tier system for plugin components — inspired by [Rust's target tier policy](https://doc.rust-lang.org/nightly/rustc/target-tier-policy.html) — to clearly define the support levels, quality requirements, and expectations for each plugin.

### Scope

This policy applies to the following plugin types in the `drasi-core` repository:

- **Source Plugins** (`components/sources/`)
- **Reaction Plugins** (`components/reactions/`)
- **Bootstrapper Plugins** (`components/bootstrappers/`)

This policy does **not** apply to:

- Identity Provider plugins (`components/identity/`)
- Index plugins (`components/indexes/`)
- State Store plugins (`components/state_stores/`)
- Core libraries (`core`, `lib`, `middleware`, `query-ast`, `query-cypher`, `query-gql`)
- Functions (`functions-cypher`, `functions-gql`)
- Plugin SDK / Host SDK / FFI Primitives (these are infrastructure, not plugins themselves)

These out-of-scope components may receive their own policies in the future.

### Terminology

This document uses the keywords "must", "must not", "should", "should not", and "may" as described in [IETF RFC 2119](https://tools.ietf.org/html/rfc2119):

- **must** / **must not** — absolute requirements
- **should** / **should not** — strong recommendations; exceptions require justification
- **may** — entirely optional

### General Principles

- This policy involves human judgment. Plugins must fulfill the **spirit** of the requirements, not just the letter, as determined by the approving maintainers.
- Neither this policy nor any decisions made regarding plugin tiers shall create any binding agreement or estoppel by any party.
- Maintainers evaluating plugins and plugin-specific patches should always use their best judgment regarding quality and suitability.
- A plugin must have already received approval for its current tier and spent a reasonable amount of time at that tier before proposing promotion to the next higher tier.

## Classification Axes

Each plugin is classified along two axes:

### Tier (Support Level)

| Tier | Name | Summary |
|------|------|---------|
| 1 | **Core** | Full support guarantees. Always builds, passes all tests, ships in official releases, documented on drasi.io. |
| 2 | **Supported** | Builds and has tests in CI. Maintained with at least basic support. May ship with official releases. |
| 3 | **Community** | Minimal guarantees. Must build and must not break other plugins. Exists for community and experimental contributions. |

### Audience

| Audience | Description |
|----------|-------------|
| **external** | User-facing plugin intended for end-users of Drasi |
| **internal** | Framework infrastructure plugin used by Drasi's own platform integration |
| **test-only** | Plugin used exclusively for testing and development |

A Tier 1 classification does not always imply the plugin is user-facing. Internal and test-only plugins may be Tier 1 if they are critical to the project's infrastructure.

## Tier 1 — Core Plugin Requirements

Tier 1 represents Drasi's highest level of support for a plugin. The project guarantees that Tier 1 plugins build, pass all tests, and ship with official releases. Patches that break a Tier 1 plugin will be rejected.

Each tier builds on all requirements from the previous tier, unless overridden by a stronger requirement. Specifically: Tier 1 must satisfy all Tier 2 and Tier 3 requirements unless explicitly overridden; Tier 2 must satisfy all Tier 3 requirements unless explicitly overridden.

### CI and Testing

- The plugin must build and pass all tests reliably in CI.
- The plugin must be included in the project's **end-to-end (E2E) test suite**.
- Building and testing the plugin must not take substantially longer than other Tier 1 plugins or raise the maintenance burden of CI infrastructure.

### Documentation

- The plugin must have comprehensive documentation published on [drasi.io](https://drasi.io), including:
  - Overview and purpose
  - Configuration and usage instructions
  - API reference (if applicable)
  - Example configurations
- The plugin must have a README in its source directory.

### SDK and Compatibility

- The plugin must use the official Drasi **Plugin SDK** (`drasi-plugin-sdk`).
- The plugin must maintain compatibility with the host SDK across **minor** host releases. Breaking compatibility requires coordination with the core team.
- Breaking changes to the plugin's public interface must follow a deprecation process and require a design document or RFC.

### Licensing

- The plugin must be licensed under **Apache-2.0**, consistent with the rest of the drasi-core repository.
- The plugin must not introduce dependencies with licenses incompatible with the project's licensing policy.
- The plugin must not cause users of Drasi to be subject to any new onerous license requirements.

### Release

- Tier 1 plugins are included in every official Drasi release.
- Tier 1 plugins follow the same versioning scheme as the drasi-core workspace.

## Tier 2 — Supported Plugin Requirements

Tier 2 plugins are maintained and functional, but carry fewer guarantees than Tier 1. The project ensures they build and their tests pass, but they may not be included in every official release or have full documentation on drasi.io.

### CI and Testing

- The plugin must build and pass its tests in CI.
- The plugin is **encouraged** (but not required) to be included in the E2E test suite.
- The plugin's maintainer(s) should regularly run the testsuite and fix failures in a reasonably timely fashion.

### Documentation

- The plugin must have a **README** in its source directory explaining:
  - Purpose and use case
  - Build and configuration instructions
  - Basic usage examples
- Documentation on drasi.io is encouraged but not required.

### SDK and Compatibility

- The plugin must use the official Drasi **Plugin SDK**.
- The plugin should maintain compatibility with the host SDK. Breaking changes require notification to the maintainers.

### Licensing

- Same requirements as Tier 1.

### Release

- Tier 2 plugins **may** be included in official Drasi releases at the project's discretion.
- Tier 2 plugins follow the workspace versioning scheme.

### Additional Requirements

- All Tier 3 requirements also apply.

## Tier 3 — Community / Experimental Plugin Requirements

Tier 3 is the entry point for new plugins. The Drasi project provides no official support for Tier 3 plugins. Requirements are minimal, focused primarily on avoiding disruption to other plugins and the project's development.

### CI and Testing

- The plugin must **build** in CI. Compilation failures will block merges.
- The plugin should have tests, but tests may be incomplete.
- Tests must not be flaky or cause CI instability for other plugins.

### Documentation

- The plugin must have a **README** in its source directory explaining:
  - Purpose
  - How to build
  - Basic usage

### SDK and Compatibility

- The plugin must use the official Drasi **Plugin SDK**.

### Licensing

- Same requirements as Tier 1.

### Non-Disruption

- Tier 3 plugins **must not** break any Tier 1 or Tier 2 plugin.
- Tier 3 plugins **must not** make breaking changes to code shared with other plugins without approval from the affected maintainers.
- Tier 3 plugins **must not impose burden** on the authors of pull requests or other developers in the community to maintain the plugin. In particular:
  - Do not post comments (automated or manual) on a PR that derail or suggest a block based on a Tier 3 plugin.
  - Do not send automated notifications to PR authors regarding a Tier 3 plugin unless they have opted in.
- Patches adding or updating Tier 3 plugins must not knowingly break another Tier 3 plugin without approval from that plugin's maintainer.

### CI Breakage and Quarantine

If a Tier 2 or Tier 3 plugin breaks CI due to an unrelated change (e.g., a Plugin SDK update or Rust toolchain upgrade), and the plugin's maintainer does not respond in a timely fashion, the project maintainers may **temporarily disable or quarantine** the plugin so that unrelated development is not blocked. This applies while plugins are in the monorepo; the plugin's maintainer will be notified and given a reasonable window to fix the issue before demotion or removal is considered.

### Removal

If a Tier 3 plugin stops meeting these requirements, or the maintainer no longer has interest or time, or the plugin shows no signs of activity and has not built for some time, the project may submit a PR to remove it. Any such PR will be communicated to the plugin's maintainer(s) and to the community before removal from a release.

## Promotion and Demotion

### Promotion

- A plugin must meet **all** requirements of the target tier before proposing promotion.
- A plugin should have spent a **reasonable amount of time** at its current tier before being proposed for promotion. The exact duration is left to the judgment of the approving maintainers, but at a minimum, multiple stable releases should typically occur between promotions.
- Promotion proposals must explicitly address how each requirement of the target tier is met.

### Demotion

- A plugin may be demoted if it no longer meets the requirements of its current tier but still meets the requirements of a lower tier.
- Any proposal for demotion will be communicated to the plugin's maintainers and the community before taking effect.
- A Tier 1 plugin will not be directly removed without first being demoted to Tier 2 or Tier 3.
- The amount of time between demotion communication and the next release may vary based on the severity of the issue and whether the demotion was planned.

### Temporary Disablement

In some circumstances, especially if plugin maintainers do not respond in a timely fashion, the project may temporarily disable a plugin in order to implement a feature not yet supported by that plugin. Such actions will include notification to the maintainers and should ideally happen early in a development cycle to give maintainers time to update.

## Code Hosting Strategy

### Current State — Monorepo

While the Drasi plugin API is being stabilized, **all plugins reside in the `drasi-core` monorepo** regardless of tier. This simplifies development, ensures API compatibility during rapid evolution, and allows the plugin SDK to be refined alongside real plugin implementations.

**Transitional rule:** While plugins remain in the monorepo, all in-scope plugins must keep repository CI green, regardless of their tier. Tier differences during this phase describe **support and release guarantees**, not permission to break shared CI.

### Future State — Split Repositories

Once the plugin API (Plugin SDK + Host SDK + FFI Primitives) reaches a stable 1.0 release:

- **Tier 1** plugins will remain in the `drasi-core` repository.
- **Tier 2** plugins may move to a dedicated plugin repository (e.g., `drasi-project/drasi-plugins`) or individual repositories under the `drasi-project` GitHub organization.
- **Tier 3** plugins may be hosted in any repository — including community-owned repositories outside the `drasi-project` organization — as long as they meet the Tier 3 requirements.

**Important:** Moving a plugin out of `drasi-core` does **not** automatically change its tier. Tier is about support and quality expectations; repository location is a hosting strategy.

The transition plan, including the exact criteria for when the API is considered stable, will be documented separately.

## Current Plugin Classification

### Merged Plugins

| Artifact | Plugin Type | Family | Tier | Audience |
|----------|-------------|--------|------|----------|
| `source/postgres` | Source | postgres | 1 | external |
| `source/http` | Source | http | 1 | external |
| `source/grpc` | Source | grpc | 1 | external |
| `source/mssql` | Source | mssql | 1 | external |
| `source/platform` | Source | platform | 1 | internal |
| `source/application` | Source | application | 1 | internal |
| `source/mock` | Source | mock | 1 | test-only |
| `bootstrap/postgres` | Bootstrapper | postgres | 1 | external |
| `bootstrap/mssql` | Bootstrapper | mssql | 1 | external |
| `bootstrap/platform` | Bootstrapper | platform | 1 | internal |
| `bootstrap/application` | Bootstrapper | application | 1 | internal |
| `bootstrap/noop` | Bootstrapper | noop | 1 | test-only |
| `bootstrap/scriptfile` | Bootstrapper | scriptfile | 2 | external |
| `reaction/http` | Reaction | http | 1 | external |
| `reaction/grpc` | Reaction | grpc | 1 | external |
| `reaction/sse` | Reaction | sse | 1 | external |
| `reaction/platform` | Reaction | platform | 1 | internal |
| `reaction/application` | Reaction | application | 1 | internal |
| `reaction/http-adaptive` | Reaction | http | 2 | external |
| `reaction/grpc-adaptive` | Reaction | grpc | 2 | external |
| `reaction/profiler` | Reaction | profiler | 2 | internal |
| `reaction/log` | Reaction | log | 1 | internal |
| `reaction/storedproc-mssql` | Reaction | storedproc | 1 | external |
| `reaction/storedproc-mysql` | Reaction | storedproc | 1 | external |
| `reaction/storedproc-postgres` | Reaction | storedproc | 1 | external |

### Open PR Plugins (Proposed Classification)

The following plugins are in open pull requests and have not yet been merged. Their proposed tier classification is included to guide acceptance and review. **These classifications are contingent on the plugin meeting all requirements of the requested tier before or at the time of merge.** This section is a point-in-time snapshot and may not reflect the current state of open PRs.

> **Note:** PRs [#199](https://github.com/drasi-project/drasi-core/pull/199) and [#210](https://github.com/drasi-project/drasi-core/pull/210) propose consolidating the existing `http`/`http-adaptive` and `grpc`/`grpc-adaptive` reaction pairs into single plugins respectively. If merged, the classification of those consolidated plugins would replace the separate entries above.

> **Note:** No open bootstrapper plugin PRs exist at the time of writing.

#### Source Plugins

| Artifact | PR | Tier | Audience | Rationale |
|----------|-----|------|----------|-----------|
| `source/oracle` | [#332](https://github.com/drasi-project/drasi-core/pull/332) | 1 | external | Enterprise database with significant user base |
| `source/mongodb` | [#251](https://github.com/drasi-project/drasi-core/pull/251) | 1 | external | Popular NoSQL database with widespread adoption |
| `source/mysql` | [#239](https://github.com/drasi-project/drasi-core/pull/239) | 1 | external | Popular relational database with widespread adoption |
| `source/mqtt` | [#350](https://github.com/drasi-project/drasi-core/pull/350) | 2 | external | IoT/messaging protocol with broad use case |
| `source/kubernetes` | [#319](https://github.com/drasi-project/drasi-core/pull/319) | 2 | external | Infrastructure monitoring; high demand |
| `source/neo4j` | [#315](https://github.com/drasi-project/drasi-core/pull/315) | 2 | external | Graph database; complements Drasi's query model |
| `source/dataverse` | [#304](https://github.com/drasi-project/drasi-core/pull/304) | 2 | external | Microsoft ecosystem integration |
| `source/sqlite` | [#280](https://github.com/drasi-project/drasi-core/pull/280) | 2 | external | Embedded database; useful for edge/dev scenarios |
| `source/ris-live` | [#330](https://github.com/drasi-project/drasi-core/pull/330) | 3 | external | Niche: BGP routing data (RIPE NCC) |
| `source/here-traffic` | [#329](https://github.com/drasi-project/drasi-core/pull/329) | 3 | external | Niche: HERE traffic data API |
| `source/cloudflare-radar` | [#328](https://github.com/drasi-project/drasi-core/pull/328) | 3 | external | Niche: Cloudflare network analytics |
| `source/gtfs-rt` | [#326](https://github.com/drasi-project/drasi-core/pull/326) | 3 | external | Niche: real-time public transit data |
| `source/open511` | [#325](https://github.com/drasi-project/drasi-core/pull/325) | 3 | external | Niche: road event / traffic data standard |
| `source/sui-deepbook` | [#324](https://github.com/drasi-project/drasi-core/pull/324) | 3 | external | Niche: Sui blockchain DeFi order book |
| `source/hyperliquid` | [#323](https://github.com/drasi-project/drasi-core/pull/323) | 3 | external | Niche: DeFi exchange data |

#### Reaction Plugins

| Artifact | PR | Tier | Audience | Rationale |
|----------|-----|------|----------|-----------|
| `reaction/rabbitmq` | [#313](https://github.com/drasi-project/drasi-core/pull/313) | 1 | external | Popular message broker with widespread adoption |
| `reaction/aws-sqs` | [#321](https://github.com/drasi-project/drasi-core/pull/321) | 1 | external | AWS messaging; major cloud ecosystem |
| `reaction/azure-storage` | [#316](https://github.com/drasi-project/drasi-core/pull/316) | 1 | external | Azure ecosystem; major cloud integration |
| `reaction/mqtt` | [#335](https://github.com/drasi-project/drasi-core/pull/335) | 2 | external | IoT/messaging protocol with broad use case |
| `reaction/mcp` | [#314](https://github.com/drasi-project/drasi-core/pull/314) | 2 | external | Model Context Protocol; AI integration |
| `reaction/file` | [#320](https://github.com/drasi-project/drasi-core/pull/320) | 2 | external | Simple file output; useful for dev/debugging |
| `reaction/loki` | [#292](https://github.com/drasi-project/drasi-core/pull/292) | 2 | external | Log aggregation (Grafana ecosystem) |
| `reaction/qdrant` | [#154](https://github.com/drasi-project/drasi-core/pull/154) | 2 | external | Niche: vector database for AI/ML |

## References

- [Rust Target Tier Policy](https://doc.rust-lang.org/nightly/rustc/target-tier-policy.html) — Inspiration for this policy
- [Drasi Governance](https://github.com/drasi-project/community/blob/main/GOVERNANCE.md) — Project governance and community membership
- [drasi-core Repository](https://github.com/drasi-project/drasi-core) — Source repository for all plugins covered by this policy
- [IETF RFC 2119](https://tools.ietf.org/html/rfc2119) — Key words for use in RFCs to indicate requirement levels
