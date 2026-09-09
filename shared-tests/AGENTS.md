# shared-tests - cross-crate conformance suite

Reusable behavioral scenarios and test harnesses consumed as a library by other crates' tests; publish = false, never released. This crate is the effective test suite for the query engine and index backends - its expectations are load-bearing far beyond this directory.

## Invisible wiring (nothing enforces this)
- New or changed use_case scenarios must be wired into ALL THREE consumer sites: src/in_memory/mod.rs, components/indexes/rocksdb/tests/scenario_tests.rs, and components/indexes/garnet/tests/scenario_tests.rs - miss one and backend coverage silently diverges
- Changing expected results breaks downstream crates: also run the tests of components/indexes/* and the reaction recovery e2e tests (components/reactions/*/tests/recovery_e2e.rs)
- redis_helpers starts Redis via testcontainers (Docker required); its 5-attempt container retry works around a Docker Desktop port-mapping bug - do not simplify it away
- recovery_test_helpers and mock_source support drasi-lib reaction-recovery tests; use_cases and temporal_retrieval target drasi-core query/index conformance
