# Native database plugin developer experience

## High-level assessment

**The native API is a credible foundation, but the current candidate is not yet a dependable, self-service experience for an external database vendor.** An experienced Rust developer can implement a small source and sink using the published SDK. Getting from that experiment to a correctly configured, recoverable, independently distributed database integration requires too much reconstruction of Drasi's own development workflow.

The strongest parts should be preserved: a lightweight plugin-side dependency boundary; shared graph-change codecs; factories separate from activation; explicit lifecycle and completion contracts; and host-owned recovery services. A native "reaction" normally means `NativeSink`, not the ordinary `Reaction` API. Both families use ComputationGraph, but they are not interchangeable SDKs.

There are also concrete defects, not just documentation polish: several database reference factories construct metadata with names where UUIDs are now required; important quickstart/configuration examples use the obsolete identity shape; and the image's bundled example has the same problem. Those failures would remain after publishing the candidate.

**Recommended order:** first repair discovery and runnable examples, then provide one complete no-checkout source/query/sink starter. Next make secret-backed configuration, supported updates, snapshot/CDC continuity and destination idempotency teachable and testable. Finally package that path as a supported third-party release workflow. More API reference material alone will not close these gaps.

### Follow-up status

The database factory-metadata panic and the prominent Server configuration
examples identified below have been repaired. The broader UUID/name cleanup and
example qualification are tracked separately.

The **standalone no-checkout native plugin starter remains explicit future
work and is not part of that cleanup**. It should be implemented only after the
checked-in examples and broader Core/Server suites are current. Its acceptance
test remains the empty-vendor-directory exercise described at the end of this
review: no sibling Drasi repositories, only published crates, tools, plugins and
images.

### Scope and confidence

The persona is an experienced Rust developer building a source and database-writing reaction for their employer's database. Crates, images and documentation are assumed published; their present publication status is **not** a finding. Drasi source is available to this reviewer as evidence, not as a prerequisite or workaround for that developer.

Evidence comes from the current working trees of `drasi-core`, `drasi-server`, `learning-drasi-server` and `test-infra`, reviewed on 7 November 2026. References below are workspace-relative paths and line ranges, including uncommitted changes, not claims about GitHub's default branches. This was a read-only source/documentation review: deterministic failure paths are distinguished from likely friction; no fresh-package build or live database/container qualification was performed.

**P0** means fix before recommending this as the external authoring path. **P1** means important for a credible production database integration. A friction finding does not mean the underlying operation is impossible.

## 1. The developer can find the SDK, but not a complete no-checkout starting point

**P0 - demonstrated onboarding gap; not an inability to implement the traits.**

The ordinary source and reaction guides correctly redirect native authors to `drasi-computation-plugin-sdk`. The native guide then explains factories, capabilities and exports, but its opening code uses undefined example factories, and its manifest fragment omits the dependency set. Counter and capture are useful small implementations, yet their instructions build packages inside the Core workspace and their manifests inherit workspace settings. The application book also starts by cloning Core and copying a path-dependent project.

The Server tutorials do not fill this gap. Getting Started explicitly requires three sibling repositories and builds Server plus seven plugins; its source and reactions are ordinary-family components, despite using the same graph runtime. Publishing those pages unchanged does not turn them into an external native-authoring tutorial.

**Impact:** the developer must assemble a project from implementation files, choose compatible dependencies, separate host dependencies from plugin dependencies, and invent the initial Server configuration before testing their database driver. Rust experience helps with the code, not with discovering which Drasi contracts are mandatory.

**Improve:** distribute a complete standalone project containing a native source, native sink, registry-versioned Cargo manifests, the export feature, a small fake database adapter, and a matching prebuilt Server/query-plugin profile. Teach one evolving example: emit a change, observe query output, write a destination record, then replace the fake adapter with the vendor driver. Keep host SDK dependencies in the harness, not the plugin. Link this path directly from the source/reaction guides and Server plugin page; label ordinary and native examples throughout.

**Evidence:** `drasi-core/components/sources/README.md:5-12`; `drasi-core/components/reactions/README.md:5-13`; `drasi-core/components/computation-plugin-sdk/README.md:10-96`; `drasi-core/components/computation-plugins/sources/counter/Cargo.toml:1-33`; `drasi-core/lib/docs/developer-guide/README.md:121-162`; `drasi-core/lib/docs/developer-guide/stockroom/Cargo.toml:16-36`; `learning-drasi-server/tutorials/getting-started/_index.md:41-89`; `learning-drasi-server/scripts/local_build.py:50-73`.

## 2. Database reference libraries currently fail before the developer reaches their database

**P0 - deterministic code blocker in the reviewed candidate.**

At review time, `ComponentId::try_new` already accepted UUIDs only, but the
database factory metadata paths still constructed name-shaped placeholder
identities. `PluginDefinition::new_with_bootstrap` invokes factory metadata while
building the library definition, so discovery failed before any database
connection or user configuration was involved.

**Follow-up:** PostgreSQL, MySQL, SQLite and SQL Server now describe their ports
through identity-independent helpers shared by metadata and real-instance
descriptors. Regression tests construct every affected public definition and
exercise the dynamic metadata export without database I/O. UUID validation,
plugin ABI and wire formats were unchanged.

**Evidence:** `drasi-core/components/computation-contracts/src/component_id.rs:55-63`; `drasi-core/components/computation-plugins/sources/postgres/src/lib.rs:260-286`; `drasi-core/components/computation-plugins/sinks/storedproc-postgres/src/lib.rs:45-70`; `drasi-core/components/computation-plugins/sources/mysql/src/lib.rs:62-84`; `drasi-core/components/computation-plugins/sinks/storedproc-mysql/src/lib.rs:62-83`; `drasi-core/components/computation-plugins/sources/sqlite/src/lib.rs:122-154`; `drasi-core/components/computation-plugins/sources/mssql/src/lib.rs:45-61`; `drasi-core/components/computation-plugin-sdk/src/export.rs:268-307`; `drasi-core/components/computation-plugin-sdk/src/lib.rs:66-91`.

## 3. The first configuration and compatibility examples are not consistently executable

**P0 - demonstrated documentation/configuration defects, plus an authoring limitation.**

The Server architecture table still advertises native ABI 1.0 and ordinary 0.16/0.17 compatibility, while the actual native ABI is 2.0.0, wire version 2, and the current ordinary ABI is 0.18.0. The Server README also tells readers to enable a removed `dynamic-plugins` Server feature. These are particularly misleading when the developer is deciding which SDK and image to combine.

The prominent readable native graph example uses `id: scene` without `name`. Current authoring requires `name` and treats optional `id` as a UUID. The Dockerfile copies `config/server-docker.yaml` into the runtime image, but that file likewise supplies name-shaped source/query/reaction IDs without names. The current source deserializer requires `name` and parses `id` as `ComponentId`. This is a startup configuration failure, not a missing release.

Identity also needs a short, accurate explanation. Readable Server authoring can resolve names among components in the same submission, then store UUID endpoints. That is **not** deferred native name binding. A separate consumer-before-producer declaration cannot persist a future name endpoint today; the supported alternative is disconnected declaration followed by explicit wiring using the assigned UUID. Do not suggest external-ID opt-in or `boundary_relationships` as a shortcut.

**Improve:** maintain one generated compatibility table and run published snippets through the current deserializers. Ship a valid minimal image configuration. Show the complete name/UUID creation receipt and wiring flow, including restart identity preservation, renames, and the distinction between accepted configuration and running components. Retain the explicit CW-93 limitation rather than borrowing ordinary name-reference semantics.

**Evidence:** `drasi-server/docs/plugin-architecture.md:8-19`; `drasi-core/components/computation-plugin-abi/src/lib.rs:26-33`; `drasi-core/components/computation-plugin-sdk/README.md:3-8`; `drasi-server/README.md:469-507`; `drasi-server/README.md:1170-1177`; `drasi-server/src/computation/authoring.rs:133-153`; `drasi-server/src/computation/authoring.rs:281-293`; `drasi-server/src/computation/authoring.rs:324-343`; `drasi-server/src/computation/authoring.rs:485-495`; `drasi-server/Dockerfile:100-116`; `drasi-server/config/server-docker.yaml:11-40`; `drasi-server/src/api/models/source.rs:116-119`; `drasi-server/src/api/models/source.rs:189-193`; `drasi-core/lib/docs/computation-graph-remaining-work.md:168-191`.

## 4. Secure database configuration takes the developer off the simple authoring path

**P1 - demonstrated API limitations and likely integration friction.**

There are three different schemas to understand: the factory's configuration fields, the port's data contract, and optional live source-schema discovery. The SDK's `ConfigSchema` is a flat field/type contract, not arbitrary JSON Schema. `ConfigDefinition` adds descriptions and selected constraints, but does not describe nested database settings. Its public `ConfigOption` builder initializes fields as non-secret and has no secret-marking method; database factories instead manually modify `ConfigSchema`.

The reference database plugins expose a whole `settings` object as secret. That is safe in intent, but its internal fields, conditional requirements and defaults are largely invisible to generated configuration forms. The recommended readable `computation.graph.config` accepts literals, whereas secret fields require unresolved references in canonical specifications. A beginner's reasonable attempt to paste the documented resolved settings object into the simple graph form is therefore rejected.

Construction-time secret resolution is also not credential rotation. The separately negotiated identity resource resolves credentials on each START, and consumer-wrapper factories currently reject identity dependencies. A vendor cannot assume the same authentication recipe works for both a normal sink and a durable external consumer.

**Improve:** give the standalone example a real secret-reference configuration and an activation-credential variant, with expected first-start/restart behavior. Provide a public secret-field builder and useful nested field descriptions. Let readable authoring express typed references without disguising reference-shaped JSON as literals, or supply a supported generator for the expanded form. Make configuration defaults, persistence, redaction and rotation semantics explicit; do not recommend removing credentials from `configuration()` and thereby corrupting reconstruction.

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/metadata.rs:22-103`; `drasi-core/components/computation-plugin-sdk/src/configuration.rs:172-245`; `drasi-core/components/computation-plugins/sinks/storedproc-postgres/src/lib.rs:45-70`; `drasi-core/components/computation-plugins/sources/postgres/README.md:35-48`; `drasi-core/components/computation-plugin-sdk/README.md:272-332`; `drasi-core/components/host-sdk/README.md:42-90`; `drasi-core/components/host-sdk/README.md:179-182`; `drasi-server/README.md:533-542`; `drasi-server/README.md:593-602`.

## 5. Snapshot plus CDC is supported in selected profiles, but the vendor must design the integration protocol

**P1 - substantial supported-but-hard work; not a claim that bootstrap is absent.**

The counter teaches emission, not database continuity. A real connector must keep snapshot/live element identities and type mapping identical, establish an authoritative snapshot frontier, retain later database history, and advance upstream acknowledgement only from the actual consumers' completed progress. Independent counters are not comparable database positions. Multiple queries add fixed membership, per-consumer progress, slow-consumer retention and ordered enrollment.

The PostgreSQL and MySQL guides explain these obligations unusually well. They also show that a standalone SELECT snapshot is deliberately weaker than coordinated CDC bootstrap. However, the material is organized around existing engine-specific implementations and host resource machinery, not a vendor-neutral, externally runnable connector-development lesson. Native source/bootstrap coordination provides a transport and ownership contract; it does not implement the new database's snapshot/live protocol for the author.

**Impact:** it is easy to produce a convincing insert/update demo while missing rows during startup, acknowledging history too early, duplicating replayed work, or mishandling a newly added query. Those errors are not prevented by compiling the SDK traits or attaching a persistent query backend.

**Improve:** teach explicit profiles: live-only with no recovery claim; coherent initial snapshot plus live changes; restartable single-consumer CDC; then multi-consumer recovery. Provide a small fake log/snapshot adapter that deterministically exercises writes during snapshot, partial transaction replay, missing history, slow consumers and membership changes. Document how the vendor supplies stable keys, physical cursor comparison, retention and failure policy. Keep unsupported combinations visibly rejected rather than hiding them behind one "durable" flag.

**Evidence:** `drasi-core/components/computation-plugins/STANDARD.md:25-34`; `drasi-core/components/computation-plugins/sources/postgres/README.md:41-48`; `drasi-core/components/computation-plugins/sources/postgres/README.md:91-166`; `drasi-core/components/computation-plugins/bootstrappers/postgres/README.md:18-39`; `drasi-core/components/computation-plugins/bootstrappers/postgres/README.md:142-186`; `drasi-core/components/computation-plugins/sources/mysql/README.md:137-166`; `drasi-core/components/computation-plugin-sdk/README.md:1085-1121`; `drasi-core/components/computation-plugin-sdk/README.md:1136-1163`.

## 6. "Supports lifecycle" does not mean the database plugin can be reconfigured

**P1 - demonstrated managed-update restrictions, not a missing lifecycle implementation.**

The lifecycle is already implemented: optional fallible initialize/start/stop/dispose and pause/resume hooks, with context binding separate from them. The SDK documents cancellation, partial initialization and retained cleanup obligations. Those are good contracts. The starter should demonstrate owned workers so the developer sees what the no-op defaults do **not** accomplish for a live database connection. Ordinary ABI 0.18 lifecycle hooks do not provide managed updates; the Learning guide correctly distinguishes explicit reaction removal/recreation from an automatic update fallback.

Managed updates require a pure factory planner, declared update modes and, for retained objects, `apply_update`. The generic Host adapter then imposes additional restrictions: it rejects construction-time input-binding factories and requires every configuration value to be literal during assessment. A database plugin following the documented secret-reference pattern therefore cannot assume that adding an update planner makes even a state-preserving settings change available through this adapter. This is separate from changing the secret behind an activation identity provider.

Core additionally rejects **every** managed `Rebootstrap` plan, including with a configured bootstrap provider. Initial bootstrap and separately configured recovery remain available. A vendor cannot promise seamless table-selection, mapping or query-semantic updates by returning `Recreate`/`Rebootstrap`. DrasiLib already owns accepted operation identity and completion-before-resume; the missing work is safe execution of the broader changes, not another plugin-owned configuration manager.

**Improve:** publish a per-change support matrix: timing-only settings, credentials on restart, reconnect/recreate, source selection, schema mapping and downstream reset. Extend safe assessment for unresolved references and actual input bindings where appropriate without resolving secrets in a pure planner. Supply cancellation/retry examples for supported updates. Track the graph-wide reset protocol as CW-92; until it exists, document an explicit operator transition preserving pending work and owned resources, not a pretend hot update. `delete_owned_resources` must remain an explicit destructive action, never automatic disposal.

**Evidence:** `drasi-core/components/computation-plugin-sdk/README.md:111-180`; `drasi-core/components/computation-plugin-sdk/README.md:217-231`; `drasi-core/components/computation-plugin-sdk/src/component.rs:74-156`; `drasi-core/components/host-sdk/src/computation/factory.rs:1273-1306`; `drasi-core/components/host-sdk/src/computation/factory.rs:1756-1771`; `drasi-core/lib/src/management/update_planning.rs:9-15`; `drasi-core/lib/docs/computation-graph-remaining-work.md:113-163`; `learning-drasi-server/README.md:90-99`.

## 7. A native database reaction needs both a data-mapping lesson and a delivery contract

**P1 - likely correctness friction; the relevant primitives already exist.**

A native sink consumes graph changes, not the ordinary reaction's `QueryResult` wire. Query rows are ordinary result nodes. Full replacement, partial patch, removed properties and identity-only deletion are distinct operations; an absent before image is not null, and projected columns do not define row identity. The standard `GraphChangeBatch` decoder is the right entry point. Copying an ordinary reaction template or assuming every delete has `before.id` is not a general native sink implementation.

Separately, `SinkCompletion::Handled` is not exactly-once delivery. A normal sink and a consumer-extension factory have different contracts. `ConsumerMode::External` supplies the host-owned delivery runner and stable `DeliveryItem::id`, but a remote effect can still repeat after success followed by a lost local progress commit. `NativeTransactionalConsumer` atomically covers host-owned state/completion; it does not enlist the vendor database automatically.

PostgreSQL's stored-procedure sink is a useful stronger reference: it commits an effect and destination receipt together, with explicit restrictions on procedure behavior. MySQL's handled sink intentionally does not promise that property. The developer needs this distinction before choosing the implementation, not after a crash test.

**Improve:** provide a database-writing example that consumes source and query envelopes through the same decoder, handles patches and deletes correctly, and uses the SDK's delivery identity rather than inventing a key from projected values or transport counters. Include the lost-commit-response case and demonstrate destination-side deduplication where the database can transact the effect and receipt together. State whether guarantees cover an operation, a batch or a source transaction; do not equate them.

**Evidence:** `drasi-core/components/computation-plugins/SINKS.md:11-40`; `drasi-core/components/computation-plugins/SINKS.md:82-118`; `drasi-core/components/computation-contracts/src/graph_view.rs:590-626`; `drasi-core/components/computation-contracts/src/component_data.rs:21-31`; `drasi-core/components/computation-plugin-sdk/README.md:1000-1048`; `drasi-core/components/computation-contracts/src/delivery.rs:58-87`; `drasi-core/components/computation-contracts/src/delivery.rs:216-253`; `drasi-core/components/computation-plugins/sinks/storedproc-postgres/README.md:15-41`; `drasi-core/components/computation-plugins/sinks/storedproc-mysql/README.md:61-73`.

## 8. Strong internal tests have not become a reusable external plugin qualification kit

**P1 - demonstrated tooling gap in the reviewed public surface.**

There are substantial native lifecycle, recovery, transaction and real-library tests. The problem is access and reuse, not an absence of testing. The SDK lifecycle harness is private test code; the Host pipeline suite locates Core workspace artifacts and hard-codes its component packages. The documented qualification commands run Core test targets. The separate E2E framework is useful for full applications and protocol endpoints, but its advertised external-Server path uses HTTP/gRPC dispatchers rather than accepting an arbitrary native library as a conformance subject.

Public `NativePlugin::from_definition` and `computation::load` allow a vendor to write a harness. They do not provide the assertions, fixtures or fault scenarios. The Host SDK's `test-support` feature forwards query-clock support; it is not itself a native plugin conformance runner.

**Improve:** ship a small test-support crate or executable that accepts the vendor's definition/library and expected fixture data. Cover metadata without I/O, configuration round-trips, exact ports/schemas, initialize-once/repeated start-stop, partial startup, pause, cancellation and retained cleanup, invalid input, bounded backpressure, duplicate delivery and actual dynamic loading. Add opt-in CDC/bootstrap and update suites. Run it on both supported Tokio runtime flavors and in a new host process after rebuilding the library. Keep live vendor-database tests as an additional layer.

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/export.rs:23-38`; `drasi-core/components/computation-plugin-sdk/src/export/lifecycle_tests.rs:25-78`; `drasi-core/components/host-sdk/tests/native_computation_pipeline.rs:4-9`; `drasi-core/components/host-sdk/tests/native_computation_pipeline.rs:55-120`; `drasi-core/components/host-sdk/Cargo.toml:9-15`; `drasi-core/lib/src/test_support.rs:4-9`; `drasi-core/components/computation-plugins/STANDARD.md:126-161`; `test-infra/e2e-test-framework/README.md:88-113`.

## 9. Native diagnostics do not get the log bridge the Server documentation appears to promise

**P1 - demonstrated integration gap; graph errors and lifecycle inspection still exist.**

The Host guide says plugin `log` and `tracing` events route into the component log registry. In the actual family loader, the native path returns before the legacy `set_log_callback` and `set_lifecycle_callback` calls. The native base ABI has no equivalent logging callback, and its SDK runtime initialization does not install a subscriber.

**Impact:** a vendor's ordinary `tracing` calls in a separately built native library do not automatically appear in Server's per-component log API through that documented bridge. Returned operation failures remain observable; this is not a claim that the graph has no diagnostics. Installing a plugin-local subscriber is a possible deliberate workaround, but then correlation, filtering and collection become vendor-specific. Rejected export definitions are another rough edge: the macro can reduce the useful construction failure to null metadata.

**Improve:** expose and document a native, instance/component-scoped diagnostic facility with bounded delivery and secret-safe messages. Provide a troubleshooting sequence that distinguishes discovery, compatibility, factory validation, waiting resources, initialization, activation and processing failures. Include a test that emits a vendor plugin diagnostic and observes it through the public host surface. Make export failures report an actionable cause. Explain that native libraries are process-pinned: rebuilding a file or recreating a component does not reload its code.

**Evidence:** `drasi-core/components/host-sdk/README.md:694-711`; `drasi-core/components/host-sdk/src/loader.rs:358-384`; `drasi-core/components/host-sdk/src/loader.rs:416-418`; `drasi-core/components/computation-plugin-abi/src/lib.rs:219-260`; `drasi-core/components/computation-plugin-sdk/src/export.rs:1925-1942`; `drasi-core/components/computation-plugin-sdk/src/lib.rs:66-91`; `drasi-core/components/computation-plugin-sdk/README.md:571-616`.

## 10. Building a cdylib is not yet a complete third-party distribution and deployment workflow

**P1 - confirmed integration constraints and likely packaging friction.**

The pieces exist: plain Cargo builds, native-family metadata, OCI installation, signature verification, plugin locking and explicit target checks. However, the documented publisher is Core's `xtask`, whose manifest is `publish = false`; its instructions assume the Core workspace. An external author can build a cdylib and manually package it, but lacks a supported end-to-end command path for doing so without the Drasi checkout.

There are several predictable traps:

| Step | Actual constraint |
|---|---|
| Name the vendor library | Default directory discovery matches `libdrasi_*` / `drasi_*`. A valid `libacme_db.so` is outside those defaults; explicit native metadata does not change the filename filter. Server warns about unmatched libraries. |
| Copy the library into Server | Verification is enabled by default. With no plugin lockfile, startup admits no plugins. A trusted local development build and a signed customer installation need different, explicit recipes. |
| Package for a container | A macOS `.dylib` cannot run in the Linux image. Native OCI compatibility requires the exact target triple and ABI; native artifacts also need an explicit filename. |
| Run a query | The ordinary query path requires the separately loaded native query plugin. A source/sink pair alone is not the complete runnable bundle. |
| Mount files | The image installs Server in `/usr/local/bin`, while the default plugin directory is beside the binary. The supplied Compose example mounts only `/app/config`; it is not a custom-plugin/data-volume recipe. |
| Upgrade | Libraries remain pinned for process lifetime. Replacing binary code requires a process restart, and new configuration/state formats must not be mistaken for automatic processing-state migration. |

These are not reasons to remove verification or loosen compatibility checks. They are reasons to make the supported path explicit. Signing policy for a company's own registry/identity, destination driver libraries, platform-specific native dependencies, writable state locations and backup ownership must be part of the vendor's deployment contract.

**Improve:** provide a versioned external packaging tool or build image, a supported platform matrix, native OCI metadata generation, and a vendor signing/installation example. Supply a minimal derived-image or mounted-plugin recipe with an explicit plugin directory, required query library, non-root permissions, persistent configuration/processing volumes, and a restart/upgrade procedure. Pin the demonstrated image and plugin bundle, not just independent `latest` tags. Release qualification should consume those artifacts exactly as the customer will.

**Evidence:** `drasi-core/xtask/Cargo.toml:1-8`; `drasi-core/README.md:212-309`; `drasi-core/components/host-sdk/src/loader.rs:775-793`; `drasi-server/src/dynamic_loading.rs:144-160`; `drasi-server/src/server.rs:103-140`; `drasi-core/components/host-sdk/src/registry/resolver.rs:372-405`; `drasi-server/README.md:394-415`; `drasi-server/Dockerfile:94-116`; `drasi-server/docker-compose-server-only.yml:30-49`; `drasi-server/src/main.rs:317-328`; `drasi-core/components/computation-plugin-sdk/README.md:610-616`.

## What would make this experience ready

The acceptance test should start in an empty vendor directory, with no sibling Drasi repositories. The developer downloads the standalone starter and uses only published crates, tools and images. They build and load the native source and sink, configure secrets, observe inserts/updates/deletes through a query, restart without identity drift, and reproduce the documented recovery and duplicate-delivery outcomes. Unsupported updates fail with an explanation and leave existing state intact. The same project produces an installable company-signed artifact and runs in the supported non-root container profile.

That exercise should be a maintained release gate, not a one-off demonstration. Existing internal tests, detailed contracts and the small native examples provide a strong base; the missing product is the coherent, independently usable path connecting them.
