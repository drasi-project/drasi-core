// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Configuration hashing for checkpoint validation.
//!
//! On query restart with a persistent index, we compare a hash of the query's
//! identity-defining config against the hash stored alongside the index. A
//! mismatch triggers an automatic wipe + re-bootstrap
//! (see design doc 02 §3 — Reading Checkpoints on Startup).
//!
//! We use `fnv` rather than `std::hash::DefaultHasher` because `DefaultHasher`
//! is explicitly documented as unstable across Rust versions — upgrading the
//! toolchain would silently invalidate every persistent query's config hash,
//! causing spurious re-bootstraps. `fnv` is a fixed algorithm, already in the
//! workspace (via the rocksdb and garnet index plugins), and deterministic
//! across versions and platforms.

use std::hash::{Hash, Hasher};

use serde::Serialize;

use crate::config::{
    QueryConfig, QueryJoinConfig, QueryJoinKeyConfig, QueryLanguage, SourceSubscriptionConfig,
};
use drasi_core::models::SourceMiddlewareConfig;

/// Minimal projection of `QueryConfig` containing only identity-defining fields.
///
/// Fields included (changes trigger index wipe + re-bootstrap):
///   - `query` text
///   - `query_language`
///   - `middleware` (order preserved — pipeline order matters)
///   - `sources` (sorted by `source_id`; within each source, `nodes` and
///     `relations` are sorted + deduped because they are consumed as `HashSet`s
///     downstream in `SubscriptionSettingsBuilder`)
///   - `joins` (sorted by `id`; within each join, `keys` are sorted because
///     each key is one side of a synthetic edge and has no inherent order)
///
/// Fields excluded (operational tuning — changes MUST NOT wipe the index):
///   - `id`, `auto_start`, `enable_bootstrap`, `bootstrap_buffer_size`,
///     `priority_queue_capacity`, `dispatch_buffer_capacity`, `dispatch_mode`,
///     `storage_backend`, `recovery_policy`.
#[derive(Serialize)]
struct QueryIdentity<'a> {
    query: &'a str,
    query_language: &'a QueryLanguage,
    middleware: &'a [SourceMiddlewareConfig],
    sources: Vec<SourceIdentity<'a>>,
    joins: Option<Vec<JoinIdentity<'a>>>,
}

/// Canonical projection of a `SourceSubscriptionConfig`.
///
/// `nodes` and `relations` are sorted and deduped to match downstream set
/// semantics. `pipeline` preserves insertion order (it is a middleware pipeline
/// where order determines which transformation runs first).
#[derive(Serialize)]
struct SourceIdentity<'a> {
    source_id: &'a str,
    nodes: Vec<&'a String>,
    relations: Vec<&'a String>,
    pipeline: &'a [String],
}

/// Canonical projection of a `QueryJoinConfig`.
///
/// `keys` are sorted by `(label, property)` because each key is an independent
/// side of a synthetic edge definition and has no inherent order.
#[derive(Serialize)]
struct JoinIdentity<'a> {
    id: &'a str,
    keys: Vec<&'a QueryJoinKeyConfig>,
}

fn canonicalize_source(source: &SourceSubscriptionConfig) -> SourceIdentity<'_> {
    let mut nodes: Vec<&String> = source.nodes.iter().collect();
    nodes.sort();
    nodes.dedup();

    let mut relations: Vec<&String> = source.relations.iter().collect();
    relations.sort();
    relations.dedup();

    SourceIdentity {
        source_id: &source.source_id,
        nodes,
        relations,
        pipeline: &source.pipeline,
    }
}

fn canonicalize_join(join: &QueryJoinConfig) -> JoinIdentity<'_> {
    let mut keys: Vec<&QueryJoinKeyConfig> = join.keys.iter().collect();
    keys.sort_by(|a, b| (&a.label, &a.property).cmp(&(&b.label, &b.property)));

    JoinIdentity { id: &join.id, keys }
}

/// Compute a deterministic hash of the identity-defining portion of a query config.
///
/// The hash is stable across processes, platforms, and Rust toolchain versions,
/// and it is invariant under cosmetic reordering of `sources`, `joins`, a
/// source's `nodes` / `relations`, and a join's `keys`.
pub fn compute_config_hash(config: &QueryConfig) -> u64 {
    let mut sources: Vec<SourceIdentity> = config.sources.iter().map(canonicalize_source).collect();
    sources.sort_by(|a, b| a.source_id.cmp(b.source_id));

    let joins = config.joins.as_ref().map(|j| {
        let mut v: Vec<JoinIdentity> = j.iter().map(canonicalize_join).collect();
        v.sort_by(|a, b| a.id.cmp(b.id));
        v
    });

    let identity = QueryIdentity {
        query: &config.query,
        query_language: &config.query_language,
        middleware: &config.middleware,
        sources,
        joins,
    };

    // `Serialize` on all included types is infallible for the data shapes we
    // use (no non-JSON-representable primitives), so the `unwrap` is safe.
    let json =
        serde_json::to_string(&identity).expect("QueryIdentity should always serialize to JSON");

    let mut hasher = fnv::FnvHasher::default();
    json.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::DispatchMode;
    use crate::config::{QueryJoinKeyConfig, SourceSubscriptionConfig};
    use crate::recovery::RecoveryPolicy;
    use drasi_core::models::SourceMiddlewareConfig;
    use serde_json::{Map, Value};
    use std::sync::Arc;

    fn base() -> QueryConfig {
        QueryConfig {
            id: "q1".into(),
            query: "MATCH (n) RETURN n".into(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![SourceSubscriptionConfig {
                source_id: "s1".into(),
                nodes: vec!["A".into()],
                relations: vec![],
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
        }
    }

    #[test]
    fn same_config_same_hash() {
        let a = base();
        let b = base();
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_query_different_hash() {
        let a = base();
        let mut b = base();
        b.query = "MATCH (m) RETURN m".into();
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_query_language_different_hash() {
        let a = base();
        let mut b = base();
        b.query_language = QueryLanguage::GQL;
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_source_id_different_hash() {
        let a = base();
        let mut b = base();
        b.sources[0].source_id = "s2".into();
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_source_labels_different_hash() {
        let a = base();
        let mut b = base();
        b.sources[0].nodes = vec!["B".into()];
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_middleware_name_different_hash() {
        let mut a = base();
        a.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m1"),
            config: Map::new(),
        }];

        let mut b = base();
        b.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m2"),
            config: Map::new(),
        }];
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_middleware_config_different_hash() {
        let mut a = base();
        a.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m1"),
            config: Map::new(),
        }];

        let mut b = base();
        let mut cfg = Map::new();
        cfg.insert("k".into(), Value::String("v".into()));
        b.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m1"),
            config: cfg,
        }];
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_join_property_different_hash() {
        let mut a = base();
        a.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![QueryJoinKeyConfig {
                label: "A".into(),
                property: "x".into(),
            }],
        }]);

        let mut b = base();
        b.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![QueryJoinKeyConfig {
                label: "A".into(),
                property: "y".into(),
            }],
        }]);
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    // ----------------------------------------------------------------
    // Operational tuning fields — MUST NOT affect the hash.
    // ----------------------------------------------------------------

    #[test]
    fn id_change_same_hash() {
        let a = base();
        let mut b = base();
        b.id = "q-other".into();
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn auto_start_change_same_hash() {
        let a = base();
        let mut b = base();
        b.auto_start = !b.auto_start;
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn enable_bootstrap_change_same_hash() {
        let a = base();
        let mut b = base();
        b.enable_bootstrap = !b.enable_bootstrap;
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn bootstrap_buffer_size_change_same_hash() {
        let a = base();
        let mut b = base();
        b.bootstrap_buffer_size = 99999;
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn priority_queue_capacity_change_same_hash() {
        let a = base();
        let mut b = base();
        b.priority_queue_capacity = Some(123456);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn dispatch_buffer_capacity_change_same_hash() {
        let a = base();
        let mut b = base();
        b.dispatch_buffer_capacity = Some(42);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn dispatch_mode_change_same_hash() {
        let a = base();
        let mut b = base();
        b.dispatch_mode = Some(DispatchMode::Broadcast);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn recovery_policy_change_same_hash() {
        let a = base();
        let mut b = base();
        b.recovery_policy = Some(RecoveryPolicy::AutoReset);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    // ----------------------------------------------------------------
    // Ordering invariance.
    // ----------------------------------------------------------------

    #[test]
    fn source_reorder_same_hash() {
        let mut a = base();
        a.sources = vec![
            SourceSubscriptionConfig {
                source_id: "s1".into(),
                nodes: vec!["A".into()],
                relations: vec![],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "s2".into(),
                nodes: vec!["B".into()],
                relations: vec![],
                pipeline: vec![],
            },
        ];

        let mut b = base();
        b.sources = vec![
            SourceSubscriptionConfig {
                source_id: "s2".into(),
                nodes: vec!["B".into()],
                relations: vec![],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "s1".into(),
                nodes: vec!["A".into()],
                relations: vec![],
                pipeline: vec![],
            },
        ];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn joins_reorder_same_hash() {
        let mut a = base();
        a.joins = Some(vec![
            QueryJoinConfig {
                id: "JA".into(),
                keys: vec![],
            },
            QueryJoinConfig {
                id: "JB".into(),
                keys: vec![],
            },
        ]);

        let mut b = base();
        b.joins = Some(vec![
            QueryJoinConfig {
                id: "JB".into(),
                keys: vec![],
            },
            QueryJoinConfig {
                id: "JA".into(),
                keys: vec![],
            },
        ]);

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn nodes_reorder_same_hash() {
        let mut a = base();
        a.sources[0].nodes = vec!["Order".into(), "Customer".into()];

        let mut b = base();
        b.sources[0].nodes = vec!["Customer".into(), "Order".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn relations_reorder_same_hash() {
        let mut a = base();
        a.sources[0].relations = vec!["PLACED_BY".into(), "CONTAINS".into()];

        let mut b = base();
        b.sources[0].relations = vec!["CONTAINS".into(), "PLACED_BY".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn duplicate_nodes_same_hash_as_deduped() {
        let mut a = base();
        a.sources[0].nodes = vec!["Order".into(), "Order".into(), "Customer".into()];

        let mut b = base();
        b.sources[0].nodes = vec!["Order".into(), "Customer".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn duplicate_relations_same_hash_as_deduped() {
        let mut a = base();
        a.sources[0].relations = vec!["R".into(), "R".into()];

        let mut b = base();
        b.sources[0].relations = vec!["R".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn join_keys_reorder_same_hash() {
        let mut a = base();
        a.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "A".into(),
                    property: "x".into(),
                },
                QueryJoinKeyConfig {
                    label: "B".into(),
                    property: "y".into(),
                },
            ],
        }]);

        let mut b = base();
        b.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "B".into(),
                    property: "y".into(),
                },
                QueryJoinKeyConfig {
                    label: "A".into(),
                    property: "x".into(),
                },
            ],
        }]);

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn pipeline_reorder_different_hash() {
        let mut a = base();
        a.sources[0].pipeline = vec!["decode".into(), "map".into()];

        let mut b = base();
        b.sources[0].pipeline = vec!["map".into(), "decode".into()];

        // Pipeline order within a source is semantically meaningful.
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn middleware_reorder_different_hash() {
        let m1 = SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("first"),
            config: Map::new(),
        };
        let m2 = SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("second"),
            config: Map::new(),
        };

        let mut a = base();
        a.middleware = vec![m1.clone(), m2.clone()];

        let mut b = base();
        b.middleware = vec![m2, m1];

        // Pipeline order is semantically meaningful — swapping changes the hash.
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }
}
