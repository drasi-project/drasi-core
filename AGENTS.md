# drasi-core

Rust workspace implementing Drasi continuous queries: Cypher/GQL queries run continuously over graph-shaped change streams and emit result diffs as source data changes, instead of one-shot query results.
Crates publish to crates.io and are consumed by downstream repos (e.g. drasi-platform, drasi-server); drasi-server also loads the plugin cdylibs published from here at runtime - public APIs, FFI layouts, and persisted formats are compatibility surfaces.

## Layout
- core/ - the query engine; query-ast, query-cypher, query-gql parse to a shared AST; functions-cypher/functions-gql bind language-specific function names
- lib/ - drasi-lib, the embedded runtime (sources -> continuous queries -> reactions, in-process)
- components/ - ~80 crates: plugins (sources, bootstrappers, reactions, indexes) plus SDKs, shared *-common libs, and the FFI backbone
- shared-tests/ - cross-crate conformance suite; middleware/ - change-stream transforms; examples/ - runnable demos
- .github/ - CI plus gh-aw agentic workflows; xtask/ - build/publish tooling that CI drives
- Deeper AGENTS.md files (core/, lib/, components/, shared-tests/, examples/) own their subtree's rules - read them before working there

## Done means (the main CI gates)
- `cargo fmt -- --check` passes
- `make clippy` passes - do NOT use `cargo clippy --all-features` (the bundled-jq feature has a flaky source build); warnings are errors
- `cargo test --workspace --exclude drasi-host-sdk` and `make test-host-sdk` pass; a running Docker daemon is required (testcontainers)
- Commits are signed off: `git commit -s` (DCO)
- CI additionally gates PRs on: a typos check, cargo-audit, cargo-deny, FFI layout tests, and coverage

## Rules automation depends on
- PRs are squash-merged and the PR title is parsed as a conventional commit by release-plz/git-cliff for version bumps and changelogs - pre-1.0, `fix:`/`feat:` bump the patch version and `feat!:` the minor
- Never hand-edit CHANGELOG.md files or crate version numbers - release-plz generates both
- Never add a dependency that installs a `#[global_allocator]` (jemalloc, mimalloc, ...) - cargo-deny CI fails the PR; host and plugin cdylibs must share the System allocator
- External contributors: the PR must link an issue the author is assigned to, or a bot flags it
- Merging the auto-generated release PR publishes to crates.io and pushes cosign-signed plugin OCI images to ghcr.io; released linux-gnu plugins are held to a CI-enforced glibc 2.28 floor, so a dependency bump alone can break the release
- In .github/workflows, *.lock.yml files are COMPILED OUTPUT of the sibling .md gh-aw workflow sources - never edit a .lock.yml; edit the .md and run `gh aw compile` (procedure: .github/workflows/readme.md)

## Build prerequisites
- Plain `cargo build`/`cargo test` need no libjq (the jq middleware feature is opt-in), but `make clippy` enables it - install system libjq with `JQ_LIB_DIR` set (setup: docs/contributing/); some plugin builds need protobuf-compiler and cmake; optional fmt pre-commit hook: `git config core.hooksPath .githooks`

## Query stack (crates at repo root, no directory of their own)
- query-cypher (openCypher) and query-gql (ISO GQL 39075 - not GraphQL) both compile to the shared AST in query-ast; core evaluates only the AST
- Function implementations live in core; functions-cypher/functions-gql only register language-specific names (sole exception: functions-gql's GQL `cast` wrapper) - implement once in core, register in both
- Parsers read the aggregating-function registry at parse time to build grouping, so registering an aggregating function changes parse results
- Lexical rules are deliberately duplicated between the two PEG grammars - mirror any fix across both
- ORDER BY / LIMIT / SKIP are intentionally unsupported (continuous queries emit diffs, not ordered snapshots) - do not add them

## Maintaining these files
- Staleness norm: if your change makes any AGENTS.md line untrue, update that line in the same PR
